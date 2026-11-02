use super::*;

impl RuntimeRepository {
    /// Persists a daemon-routed user instruction and its event together so a
    /// reconnecting observer never sees an event without its mailbox record.
    pub fn record_user_message(
        &mut self,
        sender: &TaskId,
        recipient: &TaskId,
        message: &str,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO mailbox_messages (id, recipient_task_id, sender_task_id, kind, priority, payload_json)
             VALUES (?1, ?2, ?3, 'user_instruction', 2, ?4)",
            params![
                MessageId::new().to_string(),
                recipient.to_string(),
                sender.to_string(),
                serde_json::to_string(&serde_json::json!({ "message": message }))?,
            ],
        )?;
        let event_id = append_event(&transaction, recipient, RuntimeEvent::MailboxMessageQueued)?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Persists an external user intervention without attributing it to a
    /// task. The mailbox row and audit event commit atomically.
    pub fn record_user_override_message(
        &mut self,
        recipient: &TaskId,
        message: &str,
    ) -> Result<i64, RepositoryError> {
        self.record_user_override_message_with_id(&MessageId::new(), recipient, message)
    }

    /// Uses the caller-supplied ID so the durable row and the in-memory
    /// worker inbox can acknowledge the exact same external override.
    pub fn record_user_override_message_with_id(
        &mut self,
        message_id: &MessageId,
        recipient: &TaskId,
        message: &str,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO mailbox_messages (id, recipient_task_id, sender_task_id, kind, priority, payload_json)
            VALUES (?1, ?2, NULL, 'user_override', 2, ?3)",
            params![
                message_id.to_string(),
                recipient.to_string(),
                serde_json::to_string(&serde_json::json!({ "message": message }))?,
            ],
        )?;
        let event_id = append_event(&transaction, recipient, RuntimeEvent::MailboxMessageQueued)?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Records the application worker's prompt-injection checkpoint. The
    /// row update and audit event commit atomically and are idempotent after a
    /// successful acknowledgement.
    pub fn mark_user_override_consumed(
        &mut self,
        recipient: &TaskId,
        message_id: &MessageId,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE mailbox_messages
             SET delivered_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND recipient_task_id = ?2
               AND sender_task_id IS NULL AND kind = 'user_override'
               AND delivered_at IS NULL",
            params![message_id.to_string(), recipient.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::MailboxMessageNotFound {
                message_id: message_id.to_string(),
            });
        }
        let event_id = append_event(
            &transaction,
            recipient,
            RuntimeEvent::MailboxMessageConsumed,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    pub fn mailbox_message_count(&self) -> Result<i64, RepositoryError> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM mailbox_messages", [], |row| {
                row.get(0)
            })?)
    }

    /// Commits a child delivery as the task's terminal state, together with
    /// the direct parent's high-priority mailbox notification, in one durable
    /// transaction. Delivering IS completing: there is no review wait to land
    /// in, so the task and its attempt both finish here.
    ///
    /// The emitted `task_delivered` is the terminal marker for a delivered
    /// child; a delivered child never emits `task_completed`. (v14 rewrites
    /// historical review rows to `task_completed` but does not change what new
    /// deliveries emit.)
    pub fn record_delivery(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        delivery: &DeliveryReport,
    ) -> Result<i64, RepositoryError> {
        let delivery_json = serde_json::to_string(delivery)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let parent_id = transaction
            .query_row(
                "SELECT parent_id FROM tasks WHERE id = ?1",
                [task.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::TaskNotFound {
                task: task.to_string(),
            })?
            .ok_or_else(|| RepositoryError::DeliveryRequiresParent {
                task: task.to_string(),
            })?;
        let parent: TaskId = parent_id
            .parse()
            .map_err(|_| RepositoryError::TaskNotFound {
                task: parent_id.clone(),
            })?;
        let changed = transaction.execute(
            "UPDATE tasks
             SET state_json = 'completed', delivery_json = ?1,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3 AND state_json = 'running'",
            params![delivery_json, task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotReadyForDelivery {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = 'completed'
             WHERE id = ?1 AND task_id = ?2",
            params![attempt.to_string(), task.to_string()],
        )?;
        // Delivery is a terminal state, so the child stops owning admission
        // resources in the same transaction. Leaving them behind stranded
        // `worker-worktree-context-*` / `worker-workspace-context-*` rows as
        // `active` forever: they are `exclusive`, every other terminal path
        // releases them, and nothing else revisits a delivered task's leases.
        // `transition_task_and_attempt_with_terminal` already does exactly this
        // for blocked/stalled/failed; delivery was the one terminal path that
        // did not, so a long-lived daemon accumulated one leaked pair per child.
        transaction.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE task_id = ?1 AND state = 'active'",
            params![task.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO deliveries (id, task_id, attempt_id, payload_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                delivery.id.to_string(),
                task.to_string(),
                attempt.to_string(),
                serde_json::to_string(delivery)?,
            ],
        )?;
        let message_id = MessageId::new();
        transaction.execute(
            "INSERT INTO mailbox_messages
             (id, recipient_task_id, sender_task_id, kind, priority, correlation_id, payload_json)
             VALUES (?1, ?2, ?3, 'completed', 1, ?4, ?5)",
            params![
                message_id.to_string(),
                parent.to_string(),
                task.to_string(),
                attempt.to_string(),
                serde_json::to_string(delivery)?,
            ],
        )?;
        let event_id = append_event_with_payload(
            &transaction,
            task,
            RuntimeEvent::TaskDelivered,
            &serde_json::to_string(&serde_json::json!({ "delivery_id": delivery.id }))?,
        )?;
        append_event_with_payload(
            &transaction,
            &parent,
            RuntimeEvent::MailboxMessageQueued,
            &serde_json::to_string(&serde_json::json!({
                "message_id": message_id,
                "sender_task_id": task,
                "delivery_id": delivery.id,
            }))?,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    pub fn mailbox_message_delivered_at(
        &self,
        message_id: &MessageId,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT delivered_at FROM mailbox_messages WHERE id = ?1",
                params![message_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Returns a task's durable mailbox in enqueue order without acknowledging it.
    pub fn mailbox_messages_for_task(
        &self,
        task: &TaskId,
    ) -> Result<Vec<PersistedMailboxMessage>, RepositoryError> {
        self.task_detail(task)?;
        let mut statement = self.connection.prepare(
            "SELECT id, recipient_task_id, sender_task_id, kind, priority, payload_json,
                    delivered_at, created_at
             FROM mailbox_messages
             WHERE recipient_task_id = ?1
             ORDER BY rowid",
        )?;
        statement
            .query_map(params![task.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })?
            .map(|row| {
                let (
                    message_id,
                    recipient_task_id,
                    sender_task_id,
                    kind,
                    priority,
                    payload_json,
                    delivered_at,
                    created_at,
                ) = row?;
                Ok(PersistedMailboxMessage {
                    message_id,
                    recipient_task_id: recipient_task_id.parse().map_err(|_| {
                        RepositoryError::UnknownEventKind {
                            kind: format!(
                                "invalid recipient task ID in store: {recipient_task_id}"
                            ),
                        }
                    })?,
                    sender_task_id: sender_task_id
                        .map(|sender| {
                            sender
                                .parse()
                                .map_err(|_| RepositoryError::UnknownEventKind {
                                    kind: format!("invalid sender task ID in store: {sender}"),
                                })
                        })
                        .transpose()?,
                    kind,
                    priority,
                    payload_json,
                    delivered_at,
                    created_at,
                })
            })
            .collect()
    }
}
