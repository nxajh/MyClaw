    pub fn recover_async(
        &self,
        task_id: String,
        agent_name: String,
        parent_session_id: String,
        sub_ctx: Arc<SessionContext>,
        timeout_secs: u64,
        allowed_tools: Option<Vec<String>>,
    ) {
        let (mail_tx, mail_rx) = mpsc::channel(SUB_AGENT_INBOX_CAPACITY);
        let mailbox = SubAgentMailbox {
            tx: mail_tx.clone(),
            rx: tokio::sync::Mutex::new(mail_rx),
        };
        self.mailboxes.insert(task_id.clone(), mail_tx);
        let mailboxes = Arc::clone(&self.mailboxes);

        let running = Arc::clone(&self.running);
        let event_tx = self.event_sender();
        let running_task_id = task_id.clone();
        let task_id_clone = task_id.clone();
        let session_id = parent_session_id.clone();

        let runtime = match self.runtime() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("recover_async failed to get runtime: {}", e);
                return;
            }
        };

        let handle = tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            {
                let mut session = sub_ctx.session.lock().await;
                session.sub_agent_inbox = Some(Arc::new(mailbox));
                session.turn_tool_allowlist = allowed_tools;
            }

            let turn_future = async {
                let _turn_guard = sub_ctx.turn_lock.lock().await;
                let mut session = sub_ctx.session.lock().await;
                let resolved = crate::agents::orchestrator::turn::ResolvedTurn::resolve(&session, &runtime);
                let turn_ctx = resolved.turn_context();
                sub_ctx.agent.run_recovery(&mut session, turn_ctx, &runtime).await
            };

            let result = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), turn_future).await {
                Ok(Ok(Some(tr))) if !tr.text.is_empty() => Ok(tr.text),
                Ok(Ok(_)) => Err(anyhow::anyhow!("no recovery needed or empty text")),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(anyhow::anyhow!(DelegationTimeout { secs: timeout_secs })),
            };

            let duration_secs = start_time.elapsed().as_secs();
            let timed_out_secs = result.as_ref().err().and_then(|e| e.downcast_ref::<DelegationTimeout>()).map(|t| t.secs);
            let sent_message_count = running.get(&running_task_id).map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);

            if let Some(tx) = event_tx {
                match (&result, timed_out_secs) {
                    (Ok(summary), _) => {
                        let _ = tx.send(DelegationEvent::Completed { task_id: task_id_clone, session_id, summary: summary.clone(), duration_secs, sent_message_count }).await;
                    }
                    (Err(_), Some(secs)) => {
                        let _ = tx.send(DelegationEvent::TimedOut { task_id: task_id_clone, session_id, timeout_secs: secs, duration_secs }).await;
                    }
                    (Err(e), None) => {
                        let _ = tx.send(DelegationEvent::Failed { task_id: task_id_clone, session_id, error: e.to_string() }).await;
                    }
                }
            }

            let terminal = if timed_out_secs.is_some() { DelegationStatus::TimedOut } else if result.is_ok() { DelegationStatus::Completed } else { DelegationStatus::Failed };
            if let Some(entry) = running.get(&running_task_id) {
                if let Ok(mut status) = entry.status.write() { *status = terminal; }
            }
            running.remove(&running_task_id);
            mailboxes.remove(&running_task_id);
        });

        self.running.insert(
            task_id,
            RunningEntry {
                handle,
                status: std::sync::RwLock::new(DelegationStatus::Running),
                agent_name,
                session_id: parent_session_id,
                spawned_at: std::time::Instant::now(),
                messages_sent: std::sync::atomic::AtomicU64::new(0),
            },
        );
    }