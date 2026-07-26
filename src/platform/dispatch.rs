use crate::{
    event_sourcing::{Aggregate, handle_command},
    platform::{ApiError, AppState, DomainError},
};

pub async fn dispatch<A>(state: &AppState, id: &str, command: A::Command) -> Result<A, ApiError>
where
    A: Aggregate,
    A::Error: DomainError,
    A::Command: Clone + Send + 'static,
{
    let store = state.event_store.clone();
    let bus = state.event_bus.clone();
    let id_owned = id.to_string();

    let result = state
        .queue
        .submit(id, move || async move {
            handle_command::<A>(store.as_ref(), bus.as_ref(), &id_owned, command).await
        })
        .await
        .map_err(|_| ApiError::Overloaded)?;

    result.map_err(|e| match e.downcast::<A::Error>() {
        Ok(domain_err) => ApiError::Domain(Box::new(domain_err)),
        Err(e) => ApiError::Internal(e),
    })
}
