use super::*;

/// A client-side event stream. The first frame is always a `Subscription` snapshot.
pub struct Subscription {
    pub(crate) reader: BufReader<UnixStream>,
    pub(crate) request_id: String,
}

impl Subscription {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Reads the next framed response while checking subscription correlation.
    pub fn next_frame(&mut self) -> Result<ResponseEnvelope, IpcError> {
        let response = read_limited_frame(&mut self.reader)?.ok_or_else(|| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed subscription",
            ))
        })?;
        let envelope: ResponseEnvelope = serde_json::from_slice(&response)?;
        if envelope.protocol_version != PROTOCOL_VERSION || envelope.request_id != self.request_id {
            return Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "subscription response envelope identity mismatch",
            )));
        }
        if let IpcResponse::Event(event) = &envelope.result
            && (envelope.event_id != Some(event.event_id)
                || envelope.event.as_ref() != Some(&IpcEventPayload::from(event)))
        {
            return Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "subscription event envelope identity mismatch",
            )));
        }
        Ok(envelope)
    }

    pub fn next_response(&mut self) -> Result<IpcResponse, IpcError> {
        Ok(self.next_frame()?.result)
    }
}

pub(crate) enum SubscriptionFailure {
    BeforeInitialFrame,
    AfterInitialFrame,
}

pub(crate) fn stream_subscription(
    stream: &mut UnixStream,
    database_path: &Path,
    stop: Arc<AtomicBool>,
    coordinator: Arc<RuntimeCoordinator>,
    after_event_id: i64,
    filters: SubscriptionFilters,
    request_id: &str,
) -> Result<(), SubscriptionFailure> {
    let mut repository = RuntimeRepository::open(database_path)
        .map_err(|_| SubscriptionFailure::BeforeInitialFrame)?;
    let snapshot = repository
        .subscription_snapshot(after_event_id)
        .map_err(|_| SubscriptionFailure::BeforeInitialFrame)?;
    let mut cursor = snapshot.high_water_event_id;
    write_response_frame(
        stream,
        request_id,
        None,
        &IpcResponse::Subscription(SubscriptionSnapshot {
            high_water_event_id: snapshot.high_water_event_id,
            tasks: snapshot
                .tasks
                .into_iter()
                .map(|task| IpcTask {
                    task_id: task.task_id,
                    state: task.state,
                })
                .collect(),
            events: snapshot
                .events
                .into_iter()
                .map(ipc_event)
                .filter(|event| filters.matches(event))
                .collect(),
        }),
    )
    .map_err(|_| SubscriptionFailure::AfterInitialFrame)?;

    let pending = Arc::new(PendingSubscriptionFrames::new(
        request_id,
        MAX_PENDING_EVENT_FRAMES,
    ));
    let subscription_stop = Arc::new(AtomicBool::new(false));
    let producer_pending = Arc::clone(&pending);
    let producer_stop = Arc::clone(&subscription_stop);
    let producer_daemon_stop = Arc::clone(&stop);
    let database_path = database_path.to_path_buf();
    let producer = thread::spawn(move || -> Result<(), IpcError> {
        let result = (|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            while !producer_daemon_stop.load(Ordering::Acquire)
                && !producer_stop.load(Ordering::Acquire)
            {
                runtime.block_on(coordinator.reconcile_worker_events())?;
                let repository = RuntimeRepository::open(&database_path)?;
                let events = repository.event_records_after(cursor)?;
                for event in events {
                    cursor = event.id;
                    let event = ipc_event(event);
                    if filters.matches(&event) && !producer_pending.push_event(event) {
                        return Ok(());
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })();
        producer_pending.close();
        result
    });

    let write_result = write_subscription_frames(stream, &pending, &stop);
    subscription_stop.store(true, Ordering::Release);
    pending.close();
    let producer_result = producer
        .join()
        .map_err(|_| SubscriptionFailure::AfterInitialFrame)?
        .map_err(|_| SubscriptionFailure::AfterInitialFrame);
    write_result.map_err(|_| SubscriptionFailure::AfterInitialFrame)?;
    producer_result
}

pub(crate) struct PendingSubscriptionFrames {
    request_id: String,
    capacity: usize,
    state: Mutex<PendingSubscriptionState>,
    available: Condvar,
}

pub(crate) struct PendingSubscriptionState {
    frames: VecDeque<ResponseEnvelope>,
    close_reason: SubscriptionQueueClose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubscriptionQueueClose {
    Open,
    ProducerClosed,
    Overflowed,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SubscriptionFrameWrite {
    Dropped,
    Written(usize),
}

impl PendingSubscriptionFrames {
    pub(crate) fn new(request_id: impl Into<String>, capacity: usize) -> Self {
        assert!(capacity > 0, "subscription queue capacity must be positive");
        Self {
            request_id: request_id.into(),
            capacity,
            state: Mutex::new(PendingSubscriptionState {
                frames: VecDeque::new(),
                close_reason: SubscriptionQueueClose::Open,
            }),
            available: Condvar::new(),
        }
    }

    /// Returns false once this event caused overflow or the subscription was closed.
    fn push_event(&self, event: IpcEvent) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.close_reason != SubscriptionQueueClose::Open {
            return false;
        }
        if state.frames.len() == self.capacity {
            state.frames.clear();
            state.frames.push_back(response_envelope(
                &self.request_id,
                None,
                IpcResponse::ResyncRequired,
            ));
            state.close_reason = SubscriptionQueueClose::Overflowed;
            self.available.notify_one();
            return false;
        }
        let event_id = event.event_id;
        state.frames.push_back(response_envelope(
            &self.request_id,
            Some(event_id),
            IpcResponse::Event(event),
        ));
        self.available.notify_one();
        true
    }

    /// Appends one followed trace row, with the same overflow contract as
    /// [`Self::push_event`]: the queue is bounded, and exceeding it tells the
    /// reader to resync rather than silently dropping rows.
    pub(crate) fn push_trace_row(&self, row: IpcTraceRow) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.close_reason != SubscriptionQueueClose::Open {
            return false;
        }
        if state.frames.len() == self.capacity {
            state.frames.clear();
            state.frames.push_back(response_envelope(
                &self.request_id,
                None,
                IpcResponse::ResyncRequired,
            ));
            state.close_reason = SubscriptionQueueClose::Overflowed;
            self.available.notify_one();
            return false;
        }
        let event_id = row.event_id;
        state.frames.push_back(response_envelope(
            &self.request_id,
            Some(event_id),
            IpcResponse::TraceEvent(row),
        ));
        self.available.notify_one();
        true
    }

    fn pop_wait(&self) -> Option<ResponseEnvelope> {
        let mut state = self.state.lock().unwrap();
        while state.frames.is_empty() && state.close_reason == SubscriptionQueueClose::Open {
            state = self.available.wait(state).unwrap();
        }
        state.frames.pop_front()
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap();
        if state.close_reason == SubscriptionQueueClose::Open {
            state.close_reason = SubscriptionQueueClose::ProducerClosed;
        }
        self.available.notify_all();
    }

    fn close_reason(&self) -> SubscriptionQueueClose {
        self.state.lock().unwrap().close_reason
    }

    fn write_frame_chunk(
        &self,
        writer: &mut impl Write,
        frame: &PendingFrameWrite,
    ) -> Result<SubscriptionFrameWrite, IpcError> {
        if frame.written == 0 {
            let state = self.state.lock().unwrap();
            if state.close_reason == SubscriptionQueueClose::ProducerClosed
                || (frame.is_event && state.close_reason == SubscriptionQueueClose::Overflowed)
            {
                return Ok(SubscriptionFrameWrite::Dropped);
            }
            return writer
                .write(&frame.bytes)
                .map(SubscriptionFrameWrite::Written)
                .map_err(IpcError::Io);
        }
        writer
            .write(&frame.bytes[frame.written..])
            .map(SubscriptionFrameWrite::Written)
            .map_err(IpcError::Io)
    }

    #[cfg(test)]
    fn drain_for_test(&self) -> Vec<ResponseEnvelope> {
        self.state.lock().unwrap().frames.drain(..).collect()
    }

    #[cfg(test)]
    fn is_closed(&self) -> bool {
        self.state.lock().unwrap().close_reason != SubscriptionQueueClose::Open
    }
}

pub(crate) struct PendingFrameWrite {
    bytes: Vec<u8>,
    written: usize,
    is_event: bool,
    is_resync: bool,
}

impl PendingFrameWrite {
    fn new(envelope: &ResponseEnvelope) -> Result<Self, IpcError> {
        let (mut bytes, is_event, is_resync) = encode_subscription_envelope(envelope)?;
        bytes.push(b'\n');
        Ok(Self {
            bytes,
            written: 0,
            is_event,
            is_resync,
        })
    }
}

pub(crate) fn write_subscription_frames(
    stream: &mut UnixStream,
    pending: &PendingSubscriptionFrames,
    stop: &AtomicBool,
) -> Result<(), IpcError> {
    stream.set_write_timeout(None)?;
    stream.set_nonblocking(true)?;
    let mut current: Option<PendingFrameWrite> = None;
    loop {
        let close_reason = pending.close_reason();
        if close_reason == SubscriptionQueueClose::ProducerClosed
            || (stop.load(Ordering::Acquire) && close_reason != SubscriptionQueueClose::Overflowed)
        {
            stream.shutdown(Shutdown::Write)?;
            return Ok(());
        }
        if current.is_none() {
            let Some(envelope) = pending.pop_wait() else {
                stream.shutdown(Shutdown::Write)?;
                return Ok(());
            };
            current = Some(PendingFrameWrite::new(&envelope)?);
        }

        let frame = current.as_mut().expect("pending frame was initialized");
        match pending.write_frame_chunk(stream, frame) {
            Ok(SubscriptionFrameWrite::Dropped) => {
                current = None;
            }
            Ok(SubscriptionFrameWrite::Written(0)) => {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write subscription frame",
                )));
            }
            Ok(SubscriptionFrameWrite::Written(written)) => {
                frame.written += written;
                if frame.written == frame.bytes.len() {
                    let sent_resync = frame.is_resync;
                    current = None;
                    if sent_resync {
                        stream.shutdown(Shutdown::Write)?;
                        return Ok(());
                    }
                }
            }
            Err(IpcError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod subscription_queue_tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::sync::mpsc;

    fn event(event_id: i64) -> IpcEvent {
        IpcEvent {
            event_id,
            task_id: format!("task-{event_id}"),
            kind: "task_started".into(),
            payload_json: "{}".into(),
        }
    }

    fn trace_row(event_id: i64) -> IpcTraceRow {
        IpcTraceRow {
            event_id,
            task_id: "task-1".into(),
            kind: "assistant_text".into(),
            payload_json: "{}".into(),
        }
    }

    #[test]
    fn a_trace_row_frame_is_correlated_and_an_overflow_resyncs_like_events() {
        let queue = PendingSubscriptionFrames::new("trace-client", 2);
        assert!(queue.push_trace_row(trace_row(1)));
        assert!(queue.push_trace_row(trace_row(2)));
        assert!(!queue.push_trace_row(trace_row(3)));

        let frames = queue.drain_for_test();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].request_id, "trace-client");
        assert_eq!(frames[0].event_id, None);
        assert_eq!(frames[0].result, IpcResponse::ResyncRequired);
        assert!(queue.is_closed());
    }

    #[test]
    fn a_trace_row_frame_carries_its_own_id_on_the_envelope() {
        let queue = PendingSubscriptionFrames::new("trace-client", 4);
        assert!(queue.push_trace_row(trace_row(7)));
        let frames = queue.drain_for_test();
        assert_eq!(frames[0].event_id, Some(7));
        assert!(matches!(
            &frames[0].result,
            IpcResponse::TraceEvent(row) if row.event_id == 7
        ));
        // A trace frame must encode as an event frame so the writer applies the
        // same drop-on-overflow rule as a normal event.
        let (_, is_event, is_resync) = encode_subscription_envelope(&frames[0]).unwrap();
        assert!(is_event);
        assert!(!is_resync);
    }

    #[test]
    fn pending_event_overflow_emits_one_correlated_resync_and_closes() {
        assert_eq!(MAX_PENDING_EVENT_FRAMES, 1024);
        let queue = PendingSubscriptionFrames::new("slow-client", 2);
        assert!(queue.push_event(event(1)));
        assert!(queue.push_event(event(2)));
        assert!(!queue.push_event(event(3)));
        assert!(!queue.push_event(event(4)));

        let frames = queue.drain_for_test();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].protocol_version, PROTOCOL_VERSION);
        assert_eq!(frames[0].request_id, "slow-client");
        assert_eq!(frames[0].event_id, None);
        assert_eq!(frames[0].event, None);
        assert_eq!(frames[0].result, IpcResponse::ResyncRequired);
        assert!(queue.is_closed());
    }

    #[test]
    fn slow_subscriber_receives_resync_then_socket_closes() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let queue = Arc::new(PendingSubscriptionFrames::new("slow-client", 2));
        assert!(queue.push_event(event(1)));
        assert!(queue.push_event(event(2)));
        assert!(!queue.push_event(event(3)));

        let writer_queue = Arc::clone(&queue);
        let writer = thread::spawn(move || {
            while let Some(frame) = writer_queue.pop_wait() {
                write_envelope_frame(&mut writer, &frame).unwrap();
            }
        });
        let mut subscription = Subscription {
            reader: BufReader::new(reader),
            request_id: "slow-client".into(),
        };

        assert_eq!(
            subscription.next_response().unwrap(),
            IpcResponse::ResyncRequired
        );
        assert!(matches!(
            subscription.next_response(),
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
        writer.join().unwrap();
    }

    #[test]
    fn partial_event_frame_finishes_before_resync_frame() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        set_send_buffer(&writer, 4 * 1024);
        reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let queue = Arc::new(PendingSubscriptionFrames::new("partial-client", 2));
        let mut large_event = event(1);
        large_event.task_id = "x".repeat(256 * 1024);
        assert!(queue.push_event(large_event));

        let writer_queue = Arc::clone(&queue);
        let writer = thread::spawn(move || {
            write_subscription_frames(&mut writer, &writer_queue, &AtomicBool::new(false)).unwrap();
        });
        assert!(wait_until_socket_has_bytes(&reader));
        assert!(queue.push_event(event(2)));
        assert!(queue.push_event(event(3)));
        assert!(!queue.push_event(event(4)));

        let mut subscription = Subscription {
            reader: BufReader::new(reader),
            request_id: "partial-client".into(),
        };
        let first = subscription.next_frame().unwrap();
        assert!(matches!(
            first.result,
            IpcResponse::Event(IpcEvent { event_id: 1, .. })
        ));
        assert_eq!(
            subscription.next_response().unwrap(),
            IpcResponse::ResyncRequired
        );
        assert!(matches!(
            subscription.next_response(),
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
        writer.join().unwrap();
    }

    #[test]
    fn producer_close_interrupts_a_blocked_partial_frame_writer() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        set_send_buffer(&writer, 4 * 1024);
        let queue = Arc::new(PendingSubscriptionFrames::new("closed-client", 2));
        let mut large_event = event(1);
        large_event.task_id = "x".repeat(256 * 1024);
        assert!(queue.push_event(large_event));

        let writer_queue = Arc::clone(&queue);
        let (finished_tx, finished_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let result =
                write_subscription_frames(&mut writer, &writer_queue, &AtomicBool::new(false));
            let _ = finished_tx.send(result);
        });
        assert!(wait_until_socket_has_bytes(&reader));

        queue.close();
        assert!(
            finished_rx.recv_timeout(Duration::from_millis(250)).is_ok(),
            "producer close must stop a writer blocked on an unread peer"
        );
        drop(reader);
        writer.join().unwrap();
    }

    #[test]
    fn overflow_cannot_race_between_the_initial_check_and_first_write() {
        struct LockCheckingWriter<'a> {
            state: &'a Mutex<PendingSubscriptionState>,
            observed_locked_state: bool,
            bytes: Vec<u8>,
        }

        impl Write for LockCheckingWriter<'_> {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                if self.state.try_lock().is_err() {
                    self.observed_locked_state = true;
                    return Err(std::io::ErrorKind::WouldBlock.into());
                }
                self.bytes.extend_from_slice(buffer);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let queue = PendingSubscriptionFrames::new("racing-client", 1);
        assert!(queue.push_event(event(1)));
        let envelope = queue.pop_wait().unwrap();
        let frame = PendingFrameWrite::new(&envelope).unwrap();
        let mut writer = LockCheckingWriter {
            state: &queue.state,
            observed_locked_state: false,
            bytes: Vec::new(),
        };

        let first = queue.write_frame_chunk(&mut writer, &frame);
        assert!(matches!(
            first,
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        assert!(writer.observed_locked_state);

        assert!(queue.push_event(event(2)));
        assert!(!queue.push_event(event(3)));
        assert_eq!(
            queue.write_frame_chunk(&mut writer, &frame).unwrap(),
            SubscriptionFrameWrite::Dropped
        );
        assert!(writer.bytes.is_empty());
    }

    #[test]
    fn producer_close_drops_a_frame_that_has_not_started_writing() {
        let queue = PendingSubscriptionFrames::new("closing-client", 1);
        assert!(queue.push_event(event(1)));
        let envelope = queue.pop_wait().unwrap();
        let frame = PendingFrameWrite::new(&envelope).unwrap();
        queue.close();

        let mut writer = Vec::new();
        assert_eq!(
            queue.write_frame_chunk(&mut writer, &frame).unwrap(),
            SubscriptionFrameWrite::Dropped
        );
        assert!(writer.is_empty());
    }

    #[test]
    fn normal_oversized_response_uses_a_typed_error_not_subscription_resync() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let response = response_envelope(
            "normal-client",
            None,
            IpcResponse::TaskDetail(IpcTaskDetail {
                task_id: "task".into(),
                session_id: "session".into(),
                parent_task_id: None,
                depth: 0,
                state: "queued".into(),
                delivery_json: "x".repeat(MAX_FRAME_BYTES),
                terminal_json: None,
            }),
        );

        write_envelope_frame(&mut writer, &response).unwrap();
        let mut reader = BufReader::new(reader);
        let frame = read_limited_frame(&mut reader).unwrap().unwrap();
        let response: ResponseEnvelope = serde_json::from_slice(&frame).unwrap();
        assert_eq!(response.request_id, "normal-client");
        assert_eq!(
            response.result,
            IpcResponse::Error {
                code: IpcErrorCode::Internal,
                message: None,
            }
        );
    }

    #[test]
    fn a_truncated_response_is_reported_as_truncated_not_as_oversized() {
        // A peer that closes mid-frame yields a frame without its terminator.
        // That is a *transport* failure, not a size violation; conflating the two
        // sends operators chasing a payload-size limit that was never hit.
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all(b"{\"partial\":").unwrap();
        writer.shutdown(Shutdown::Write).unwrap();

        let mut reader = BufReader::new(reader);
        let error = read_limited_frame(&mut reader).unwrap_err();

        assert!(
            matches!(error, IpcError::TruncatedFrame { .. }),
            "a closed mid-frame response must be TruncatedFrame, got: {error:?}"
        );
    }

    #[test]
    fn a_frame_over_the_cap_is_reported_as_too_large_not_truncated() {
        // The genuine size violation keeps its own distinct error.
        //
        // The payload is larger than the socket buffer, so it has to be written
        // from its own thread: a same-thread write fills the buffer and blocks
        // before this thread ever reaches read_limited_frame, hanging the test
        // instead of exercising the error path.
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let pump = std::thread::spawn(move || {
            let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
            writer.write_all(&oversized).unwrap();
            writer.write_all(b"\n").unwrap();
            writer.flush().unwrap();
        });

        let mut reader = BufReader::new(reader);
        let error = read_limited_frame(&mut reader).unwrap_err();

        assert!(
            matches!(error, IpcError::FrameTooLarge),
            "a frame past the cap must stay FrameTooLarge, got: {error:?}"
        );
        pump.join().unwrap();
    }

    #[test]
    fn truncation_and_write_timeout_map_to_validation_not_internal() {
        assert_eq!(
            ipc_error_code(&IpcError::TruncatedFrame { received: 12 }),
            IpcErrorCode::Validation
        );
        assert_eq!(
            ipc_error_code(&IpcError::FrameWriteTimeout {
                written: 8192,
                total: 26000,
            }),
            IpcErrorCode::Validation
        );
    }

    #[test]
    fn subscription_failure_after_initial_frame_does_not_append_an_error_frame() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(br#"{"partial":"#).unwrap();

        respond_to_subscription_result(
            &mut writer,
            "subscription-client",
            Err(SubscriptionFailure::AfterInitialFrame),
        )
        .unwrap();
        writer.shutdown(Shutdown::Write).unwrap();

        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, br#"{"partial":"#);
    }

    #[test]
    fn draining_admission_error_is_a_typed_invalid_state_response() {
        assert_eq!(
            ipc_error_code(&IpcError::Runtime(RuntimeCoordinatorError::Draining)),
            IpcErrorCode::InvalidState
        );
    }

    fn set_send_buffer(stream: &UnixStream, bytes: libc::c_int) {
        // SAFETY: the file descriptor and option pointer are valid for this call.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                std::ptr::addr_of!(bytes).cast(),
                std::mem::size_of_val(&bytes) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0);
    }

    #[test]
    fn a_frame_larger_than_the_send_buffer_is_written_in_full() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        set_send_buffer(&writer, 4 * 1024);

        let payload = vec![b'x'; 64 * 1024];
        let deadline = Instant::now() + Duration::from_secs(10);

        // Drain until EOF rather than a byte count: the frame is payload plus
        // its terminator, so stopping at `payload.len()` closes the reader
        // before the trailing newline is written and the writer sees BrokenPipe.
        let pump = std::thread::spawn(move || {
            let mut drained = 0usize;
            let mut scratch = [0u8; 8192];
            loop {
                match reader.read(&mut scratch) {
                    Ok(0) => break,
                    Ok(n) => drained += n,
                    Err(_) => break,
                }
            }
            drained
        });

        write_frame_until(&mut writer, &payload, deadline).unwrap();
        write_frame_until(&mut writer, b"\n", deadline).unwrap();
        writer.flush().unwrap();
        drop(writer);

        let drained = pump.join().unwrap();
        assert_eq!(
            drained,
            payload.len() + 1,
            "the whole frame, terminator included, must reach the peer"
        );
    }

    fn wait_until_socket_has_bytes(stream: &UnixStream) -> bool {
        for _ in 0..1_000 {
            let mut byte = 0_u8;
            // SAFETY: the one-byte output buffer and file descriptor are valid.
            let received = unsafe {
                libc::recv(
                    stream.as_raw_fd(),
                    std::ptr::addr_of_mut!(byte).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            if received > 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }
}

#[cfg(test)]
mod attach_application_root_wire_tests {
    use super::*;

    /// An older client does not send the field at all, and that must keep
    /// parsing as "no linked roots" rather than failing the request.
    #[test]
    fn attach_application_root_defaults_missing_extra_roots_to_empty() {
        let raw = r#"{"type":"AttachApplicationRoot","idempotency_key":"k","workspace":"/tmp/w"}"#;
        let request: IpcRequest = serde_json::from_str(raw).unwrap();
        match request {
            IpcRequest::AttachApplicationRoot { extra_roots, .. } => {
                assert!(
                    extra_roots.is_empty(),
                    "missing extra_roots must read as empty"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The linked roots a caller does send must survive the wire verbatim.
    #[test]
    fn attach_application_root_carries_extra_roots_over_the_wire() {
        let request = IpcRequest::AttachApplicationRoot {
            idempotency_key: "k".into(),
            workspace: PathBuf::from("/tmp/w"),
            extra_roots: vec!["/w/linked-b".into(), "/w/linked-c".into()],
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<IpcRequest>(&encoded).unwrap(),
            request
        );
    }
}
