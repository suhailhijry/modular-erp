use anyhow::anyhow;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    accounting::{Account, AccountCommand, AccountError},
    event_sourcing::load_aggregate,
    platform::{ApiError, AppState, DomainError, dispatch},
};

impl DomainError for AccountError {
    fn status_code(&self) -> StatusCode {
        match self {
            AccountError::AlreadyOpen => StatusCode::CONFLICT,
            AccountError::InsufficientFunds { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }
}

#[derive(Deserialize)]
pub struct OpenAccountBody {
    owner: String,
}

#[derive(Deserialize)]
pub struct AmountBody {
    amount: u64,
}

#[derive(Serialize)]
pub struct AccountResponse {
    id: String,
    owner: String,
    balance: u64,
}

impl From<Account> for AccountResponse {
    fn from(a: Account) -> Self {
        Self {
            id: a.id,
            owner: a.owner,
            balance: a.balance,
        }
    }
}

async fn open_account_handler(
    State(state): State<AppState>,
    Json(body): Json<OpenAccountBody>,
) -> Result<Json<AccountResponse>, ApiError> {
    let id = Uuid::now_v7();

    let account = dispatch::<Account>(
        &state,
        &id.to_string(),
        AccountCommand::Open {
            id: id.to_string(),
            owner: body.owner,
        },
    )
    .await?;
    Ok(Json(account.into()))
}

async fn withdraw_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(body): Json<AmountBody>,
) -> Result<Json<AccountResponse>, ApiError> {
    let account = dispatch::<Account>(
        &state,
        &id,
        AccountCommand::Withdraw {
            amount: body.amount,
        },
    )
    .await?;
    Ok(Json(account.into()))
}

async fn deposit_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(body): Json<AmountBody>,
) -> Result<Json<AccountResponse>, ApiError> {
    let account = dispatch::<Account>(
        &state,
        &id,
        AccountCommand::Deposit {
            amount: body.amount,
        },
    )
    .await?;
    Ok(Json(account.into()))
}

async fn get_account_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<AccountResponse>, ApiError> {
    let store = state.event_store.clone();
    let account = load_aggregate::<Account>(store.as_ref(), &id.to_string()).await;
    match account {
        Ok(account) => Ok(Json(account.into())),
        Err(error) => Err(ApiError::Internal(anyhow!(error))),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/accounts", post(open_account_handler))
        .route("/accounts/{id}/withdraw", post(withdraw_handler))
        .route("/accounts/{id}/deposit", post(deposit_handler))
        .route("/accounts/{id}", get(get_account_handler))
}
