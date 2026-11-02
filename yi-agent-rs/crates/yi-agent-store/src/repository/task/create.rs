use super::*;

impl RuntimeRepository {
    pub fn create_task(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        state: &str,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        // The compatibility entry point creates an otherwise-empty session snapshot.
        transaction.execute(
            "INSERT INTO sessions (id, project_root, state, config_json) VALUES (?1, '', 'active', '{}') ON CONFLICT(id) DO NOTHING",
            params![root.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, NULL, 0, ?3, 1, '', '{}')",
            params![task.to_string(), root.to_string(), state],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Creates a task and its active attempt in one transaction. Runtime-owned
    /// tasks use this path so crash recovery never sees an empty attempt ID.
    pub fn create_task_with_attempt(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
    ) -> Result<(), RepositoryError> {
        self.create_task_with_attempt_and_objective(
            task,
            root,
            attempt,
            attempt_number,
            state,
            "Root session objective not specified.",
            // Root tasks own an isolated session worktree, so this convenience
            // wrapper keeps the coding mode.
            ChildWriteMode::Coding,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_task_with_attempt_and_objective(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
        objective: &str,
        workspace_mode: ChildWriteMode,
        model: Option<String>,
        inherited_sandbox: Option<InheritedSandbox>,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        let delivery_json = serde_json::to_string(&serde_json::json!({ "objective": objective }))?;
        transaction.execute(
            "INSERT INTO sessions (id, project_root, state, config_json) VALUES (?1, '', 'active', '{}') ON CONFLICT(id) DO NOTHING",
            params![root.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json, workspace_mode, model, inherited_sandbox)
             VALUES (?1, ?2, NULL, 0, ?3, 1, ?4, ?5, ?6, ?7, ?8)",
            params![
                task.to_string(),
                root.to_string(),
                state,
                attempt.to_string(),
                delivery_json,
                workspace_mode.as_str(),
                model,
                inherited_sandbox.map(|value| value.as_str()),
            ],
        )?;
        insert_attempt(&transaction, attempt, task, attempt_number, state)?;
        transaction.commit()?;
        Ok(())
    }

    /// Legacy/compat child insert that does not set `workspace_mode`; it
    /// therefore defaults to `'coding'`. Do not use for subagent children —
    /// use `create_child_task_with_attempt_and_objective` and pass the mode.
    pub fn create_child_task(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        parent: &TaskId,
        depth: u8,
        state: &str,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, '', '{}')",
            params![
                task.to_string(),
                root.to_string(),
                parent.to_string(),
                depth,
                state,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_child_task_with_attempt(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        parent: &TaskId,
        depth: u8,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
    ) -> Result<(), RepositoryError> {
        self.create_child_task_with_attempt_and_objective(
            task,
            root,
            parent,
            depth,
            attempt,
            attempt_number,
            state,
            "Complete the delegated task.",
            // Legacy compat wrapper used only by tests; callers needing a
            // read-only child go through `create_child_task_with_attempt_and_objective`.
            ChildWriteMode::Coding,
            None,
            // No conversation marker and no inherited sandbox: this legacy
            // wrapper predates both.
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_child_task_with_attempt_and_objective(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        parent: &TaskId,
        depth: u8,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
        objective: &str,
        workspace_mode: ChildWriteMode,
        model: Option<String>,
        thread_id: Option<&str>,
        inherited_sandbox: Option<InheritedSandbox>,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        let delivery_json = serde_json::to_string(&serde_json::json!({ "objective": objective }))?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json, workspace_mode, model, thread_id, inherited_sandbox)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                task.to_string(),
                root.to_string(),
                parent.to_string(),
                depth,
                state,
                attempt.to_string(),
                delivery_json,
                workspace_mode.as_str(),
                model,
                thread_id,
                inherited_sandbox.map(|value| value.as_str()),
            ],
        )?;
        insert_attempt(&transaction, attempt, task, attempt_number, state)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_attempt(
        &mut self,
        attempt: &AttemptId,
        task: &TaskId,
        number: u32,
        state: &str,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "INSERT INTO attempts (id, task_id, number, state, budget_json, usage_json)
             VALUES (?1, ?2, ?3, ?4, '{}', '{}')",
            params![attempt.to_string(), task.to_string(), number, state],
        )?;
        Ok(())
    }
}
