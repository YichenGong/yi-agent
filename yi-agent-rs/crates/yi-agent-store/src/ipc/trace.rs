use super::*;

/// Reads a task's retained trace: the rows plus the cursor to resume from.
pub fn read_task_trace(
    socket_path: impl AsRef<Path>,
    task_id: &str,
) -> Result<TraceSnapshot, IpcError> {
    match into_result(send_request(
        socket_path,
        IpcRequest::ReadTaskTrace {
            task_id: task_id.to_owned(),
        },
    )?)? {
        IpcResponse::TaskTrace {
            high_water_id,
            rows,
            ..
        } => Ok(TraceSnapshot {
            high_water_id,
            rows,
        }),
        other => Err(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("daemon returned a non-trace response: {other:?}"),
        ))),
    }
}

/// Follows one or more tasks' traces live.
///
/// `task_ids` must be non-empty: the daemon rejects an unscoped subscription
/// rather than guessing at "every task".
pub fn subscribe_trace(
    socket_path: impl AsRef<Path>,
    after_id: i64,
    task_ids: &[String],
    kinds: &[String],
) -> Result<TraceSubscription, IpcError> {
    let mut stream = UnixStream::connect(socket_path)?;
    let request_id = next_request_id();
    write_request(
        &mut stream,
        PROTOCOL_VERSION,
        request_id.clone(),
        IpcRequest::SubscribeTrace {
            task_ids: task_ids.to_vec(),
            after_id: Some(after_id),
            kinds: kinds.to_vec(),
        },
    )?;
    Ok(TraceSubscription {
        reader: BufReader::new(stream),
        request_id,
        started: false,
    })
}

/// A client-side trace stream. The first frame is always a `TraceSubscription`
/// snapshot, which `next_row` consumes before yielding any row.
pub struct TraceSubscription {
    reader: BufReader<UnixStream>,
    request_id: String,
    /// The snapshot frame is consumed lazily so a caller can still inspect the
    /// frame it arrived in.
    started: bool,
}

impl TraceSubscription {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Make the underlying socket non-blocking.
    ///
    /// A UI that must keep repainting cannot afford to block in `next_row`, so
    /// it puts the stream in non-blocking mode and calls [`Self::try_row`]
    /// instead: each read returns immediately and a `WouldBlock` means "no row
    /// yet", which the caller turns into another frame. The mode is a property
    /// of the socket, so it applies to the reads `next_row` also performs.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<(), IpcError> {
        self.reader.get_ref().set_nonblocking(nonblocking)?;
        Ok(())
    }

    /// A row if one is buffered right now, `None` if none has arrived.
    ///
    /// Only meaningful on a non-blocking subscription; on a blocking one the
    /// read waits for the next frame, which is exactly what `next_row` does.
    /// The end of the stream is still reported as `None`, indistinguishable
    /// from "nothing yet" by design: the caller can keep its last view.
    pub fn try_row(&mut self) -> Result<Option<IpcTraceRow>, IpcError> {
        match self.next_row() {
            Ok(row) => Ok(row),
            Err(IpcError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Reads one frame and checks its correlation, mirroring `Subscription`.
    pub fn next_frame(&mut self) -> Result<ResponseEnvelope, IpcError> {
        let response = read_limited_frame(&mut self.reader)?.ok_or_else(|| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed the trace subscription",
            ))
        })?;
        let envelope: ResponseEnvelope = serde_json::from_slice(&response)?;
        if envelope.protocol_version != PROTOCOL_VERSION || envelope.request_id != self.request_id {
            return Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "trace subscription envelope identity mismatch",
            )));
        }
        if let IpcResponse::TraceEvent(row) = &envelope.result
            && envelope.event_id != Some(row.event_id)
        {
            return Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "trace subscription event envelope identity mismatch",
            )));
        }
        if let IpcResponse::Error { code, message } = &envelope.result {
            return Err(IpcError::Remote {
                code: *code,
                message: message.clone().unwrap_or_else(|| code.to_string()),
            });
        }
        Ok(envelope)
    }

    /// The next streamed row, or `None` once the stream ends.
    ///
    /// The snapshot frame is consumed on the first call, so a caller that only
    /// wants rows never has to know the snapshot exists. A stream that overflows
    /// closes as it does for events, which surfaces here as `None` after the
    /// daemon's resync frame.
    pub fn next_row(&mut self) -> Result<Option<IpcTraceRow>, IpcError> {
        if !self.started {
            self.started = true;
            match self.next_frame()?.result {
                IpcResponse::TraceSubscription(_) => {}
                IpcResponse::ResyncRequired => return Ok(None),
                other => {
                    return Err(IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("trace subscription began with {other:?}"),
                    )));
                }
            }
        }
        // Every outcome of one frame is terminal for this call: a row, the end
        // of the stream, or an error. `next_row` is therefore a single read, not
        // a loop; a caller that wants to keep going just calls it again.
        match self.next_frame() {
            Ok(envelope) => match envelope.result {
                IpcResponse::TraceEvent(row) => Ok(Some(row)),
                IpcResponse::ResyncRequired => Ok(None),
                other => Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unexpected frame on a trace subscription: {other:?}"),
                ))),
            },
            // A closed stream is the documented end of the subscription.
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

/// The rows a trace subscriber resumes from, plus the cursor it resumes at.
///
/// The rows are read first and the cursor is the largest id actually returned
/// (never a separately sampled watermark), so a row committed while this runs is
/// either inside `rows` or strictly after the cursor — never both, and never
/// neither. Sampling a watermark first would leave a window in which a row is
/// already visible to the cursor yet absent from `rows`, which is exactly how a
/// subscriber would lose or double-report it.
///
/// Row ids are global (one `AUTOINCREMENT` sequence for the whole table) and
/// never reused, so this single cursor orders every subscribed task's rows.
pub(crate) fn trace_snapshot(
    repository: &RuntimeRepository,
    task_ids: &[TaskId],
    after_id: i64,
    kinds: &[String],
) -> Result<TraceSnapshot, IpcError> {
    let mut rows = Vec::new();
    for task_id in task_ids {
        for row in repository.trace_after(task_id, after_id)? {
            if kinds.is_empty() || kinds.iter().any(|kind| kind == &row.kind) {
                rows.push(ipc_trace_row(row));
            }
        }
    }
    rows.sort_by_key(|row| row.event_id);
    let high_water_id = rows.last().map(|row| row.event_id).unwrap_or(0);
    Ok(TraceSnapshot {
        high_water_id,
        rows,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn stream_trace_subscription(
    stream: &mut UnixStream,
    database_path: &Path,
    stop: Arc<AtomicBool>,
    coordinator: Arc<RuntimeCoordinator>,
    task_ids: Vec<TaskId>,
    after_id: i64,
    kinds: Vec<String>,
    request_id: &str,
) -> Result<(), SubscriptionFailure> {
    let repository = RuntimeRepository::open(database_path)
        .map_err(|_| SubscriptionFailure::BeforeInitialFrame)?;
    let snapshot = trace_snapshot(&repository, &task_ids, after_id, &kinds)
        .map_err(|_| SubscriptionFailure::BeforeInitialFrame)?;
    let mut cursor = snapshot.high_water_id;
    write_response_frame(
        stream,
        request_id,
        None,
        &IpcResponse::TraceSubscription(snapshot),
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
                // Rows are filtered per subscribed task but the cursor is the
                // global row id, so a task's own rows cannot advance it past
                // another task's unseen rows.
                let mut fresh: Vec<IpcTraceRow> = Vec::new();
                for task_id in &task_ids {
                    // `task_id` is a `TaskId`, already validated by the caller.
                    for row in repository.trace_after(task_id, cursor)? {
                        fresh.push(ipc_trace_row(row));
                    }
                }
                fresh.sort_by_key(|row| row.event_id);
                for row in fresh {
                    cursor = row.event_id;
                    let wanted = kinds.is_empty() || kinds.iter().any(|kind| kind == &row.kind);
                    if wanted && !producer_pending.push_trace_row(row) {
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

#[cfg(test)]
mod trace_subscription_tests {
    use super::*;

    struct Server {
        writer: UnixStream,
        request_id: String,
    }

    impl Server {
        fn send(&mut self, event_id: Option<i64>, response: IpcResponse) {
            write_envelope_frame(
                &mut self.writer,
                &response_envelope(&self.request_id, event_id, response),
            )
            .unwrap();
        }
    }

    fn pair(request_id: &str) -> (TraceSubscription, Server) {
        let (server, client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        (
            TraceSubscription {
                reader: BufReader::new(client),
                request_id: request_id.into(),
                started: false,
            },
            Server {
                writer: server,
                request_id: request_id.into(),
            },
        )
    }

    fn row(event_id: i64) -> IpcTraceRow {
        IpcTraceRow {
            event_id,
            task_id: "task-1".into(),
            kind: "assistant_text".into(),
            payload_json: "{}".into(),
        }
    }

    #[test]
    fn a_trace_subscription_rejects_a_frame_for_another_request() {
        let (mut subscription, mut server) = pair("mine");
        server.request_id = "someone-else".into();
        server.send(
            None,
            IpcResponse::TraceSubscription(TraceSnapshot {
                high_water_id: 0,
                rows: Vec::new(),
            }),
        );
        assert!(matches!(
            subscription.next_row(),
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn a_trace_subscription_consumes_the_snapshot_then_yields_rows() {
        let (mut subscription, mut server) = pair("mine");
        server.send(
            None,
            IpcResponse::TraceSubscription(TraceSnapshot {
                high_water_id: 2,
                rows: vec![row(1), row(2)],
            }),
        );
        server.send(Some(3), IpcResponse::TraceEvent(row(3)));
        server.send(Some(4), IpcResponse::TraceEvent(row(4)));
        drop(server.writer);

        assert_eq!(subscription.next_row().unwrap().unwrap().event_id, 3);
        assert_eq!(subscription.next_row().unwrap().unwrap().event_id, 4);
        assert!(subscription.next_row().unwrap().is_none());
    }

    #[test]
    fn a_trace_subscription_rejects_a_row_whose_envelope_id_disagrees() {
        let (mut subscription, mut server) = pair("mine");
        server.send(
            None,
            IpcResponse::TraceSubscription(TraceSnapshot {
                high_water_id: 0,
                rows: Vec::new(),
            }),
        );
        // Envelope says 9, the row says 10.
        server.send(Some(9), IpcResponse::TraceEvent(row(10)));
        assert!(matches!(
            subscription.next_row(),
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn a_trace_subscription_surfaces_a_daemon_error_frame() {
        let (mut subscription, mut server) = pair("mine");
        server.send(
            None,
            IpcResponse::Error {
                code: IpcErrorCode::Validation,
                message: Some("a trace subscription requires at least one valid task id".into()),
            },
        );
        assert!(matches!(
            subscription.next_row(),
            Err(IpcError::Remote {
                code: IpcErrorCode::Validation,
                ..
            })
        ));
    }

    #[test]
    fn a_trace_subscription_treats_resync_as_the_end_of_the_stream() {
        let (mut subscription, mut server) = pair("mine");
        server.send(
            None,
            IpcResponse::TraceSubscription(TraceSnapshot {
                high_water_id: 0,
                rows: Vec::new(),
            }),
        );
        server.send(None, IpcResponse::ResyncRequired);
        assert!(subscription.next_row().unwrap().is_none());
    }

    #[test]
    fn a_trace_subscription_does_not_mistake_a_resync_first_frame_for_a_snapshot() {
        let (mut subscription, mut server) = pair("mine");
        server.send(None, IpcResponse::ResyncRequired);
        assert!(subscription.next_row().unwrap().is_none());
    }

    #[test]
    fn a_daemon_error_frame_becomes_a_remote_error_not_a_response() {
        let error = into_result(IpcResponse::Error {
            code: IpcErrorCode::AuthorityDenied,
            message: None,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            IpcError::Remote {
                code: IpcErrorCode::AuthorityDenied,
                ..
            }
        ));
        assert!(into_result(IpcResponse::ResyncRequired).is_ok());
    }
}
