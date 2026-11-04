//! The accounts messages: the read-only half. `ListAccounts` is served on
//! every connection origin, since a snapshot carries no secret. Until the
//! daemon can read the account store, the answer is empty and carries no
//! auto-switch state.
use super::connection::ConnCtx;
use protocol::{ClientMessage, DaemonMessage};
use tracing::warn;

/// Handle the accounts messages `dispatch` routes here.
pub(super) fn handle(msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::ListAccounts { request_id } => {
            let _ = ctx.out_tx.send(DaemonMessage::Accounts {
                accounts: Vec::new(),
                auto: None,
                request_id,
            });
        }
        other => warn!(?other, "accounts::handle: routed a message it does not own"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::connection::AuthKind;
    use crate::server::test_support::{dispatch_one_as, test_hub, test_peer};

    #[tokio::test]
    async fn list_accounts_answers_empty_from_both_origins() {
        let (hub, _scratch) = test_hub("list-accounts-empty");
        for auth in [AuthKind::Local, AuthKind::Tailnet(test_peer())] {
            let reply = dispatch_one_as(
                &hub,
                ClientMessage::ListAccounts {
                    request_id: Some("acc-1".to_owned()),
                },
                &auth,
            )
            .await;
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Accounts { accounts, auto: None, request_id: Some(id) }
                        if accounts.is_empty() && id == "acc-1"
                ),
                "{auth:?}: {reply:?}"
            );
        }
    }
}
