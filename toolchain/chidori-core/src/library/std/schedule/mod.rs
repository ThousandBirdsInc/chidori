//

enum ScheduledExecutionError {
    None,
}

#[async_trait]
pub trait ScheduledExecutionHost<C> {
    // Connects to the vector database
    fn attach_client(client: C) -> Result<Self, ScheduledExecutionError>
    where
        Self: Sized;

    // Inserts a vector into the database
    async fn schedule_cron_event(
        &mut self,
        cron_tab: String,
    ) -> Result<(), ScheduledExecutionError>;
}
