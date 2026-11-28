// SPDX-License-Identifier: Apache-2.0

//! The protocol routes of `coqo.agents@1` (`agents/*`, `sessions/*`), with
//! the workspaces' permission rules (`agents/permissionRules`,
//! `agents/deletePermissionRule`).

use coqo_protocol::methods::agents::CriterionAuthor;
use coqo_protocol::methods::agents::{
    Cancel, Changes, Close, Create, DeletePermissionRule, Get, List, PermissionRules, Prompt,
    Providers, Rename, RespondPermission, RunAction, SetConfigOption, SetCriteria, SetMode,
    SetStatus, Updates,
};
use coqo_protocol::{Access, Router, Scope};

use crate::manager::SessionManager;

/// Adds the agent methods and topics, served by `manager`. All of them but
/// two need the daemon token: they reach every session and workspace, and
/// run provider actions, so a session token (`coqod mcp`) gets `FORBIDDEN`.
/// The exceptions are `sessions/setStatus` and `sessions/setCriteria`,
/// which a session token may call for its own session (the `set_status`
/// and `set_acceptance_criteria` MCP tools). A session token's criteria
/// changes are the agent's, the daemon token's the user's.
pub fn register(router: Router, manager: SessionManager) -> Router {
    let m = manager;
    let providers = m.clone();
    let run_action = m.clone();
    let list = m.clone();
    let get = m.clone();
    let create = m.clone();
    let prompt = m.clone();
    let cancel = m.clone();
    let respond = m.clone();
    let set_mode = m.clone();
    let set_config = m.clone();
    let set_status = m.clone();
    let set_criteria = m.clone();
    let rename = m.clone();
    let close = m.clone();
    let rules = m.clone();
    let delete_rule = m.clone();
    let updates = m.clone();
    let changes = m;
    router
        .method_with::<Providers, _, _>(Access::daemon(), move |_, ()| {
            let r = providers.providers();
            async move { Ok(r) }
        })
        .method_with::<RunAction, _, _>(Access::daemon(), move |_, p| {
            let m = run_action.clone();
            async move {
                m.run_action(&p.provider, p.action)
                    .await
                    .map_err(Into::into)
            }
        })
        .method_with::<List, _, _>(Access::daemon(), move |_, p| {
            let r = list.list(&p).map_err(Into::into);
            async move { r }
        })
        .method_with::<Get, _, _>(Access::daemon(), move |_, p| {
            let r = get.snapshot(&p.session).map_err(Into::into);
            async move { r }
        })
        .method_with::<Create, _, _>(Access::daemon(), move |_, p| {
            let m = create.clone();
            async move { m.create(p).await.map_err(Into::into) }
        })
        .method_with::<Prompt, _, _>(Access::daemon(), move |_, p| {
            let m = prompt.clone();
            async move { m.prompt(&p.session, p.content).await.map_err(Into::into) }
        })
        .method_with::<Cancel, _, _>(Access::daemon(), move |_, p| {
            let m = cancel.clone();
            async move { m.cancel(&p.session).await.map_err(Into::into) }
        })
        .method_with::<RespondPermission, _, _>(Access::daemon(), move |_, p| {
            let r = respond
                .answer_permission(&p.session, p.request_id, p.outcome, p.remember)
                .map_err(Into::into);
            async move { r }
        })
        .method_with::<SetMode, _, _>(Access::daemon(), move |_, p| {
            let m = set_mode.clone();
            async move { m.set_mode(&p.session, p.mode_id).await.map_err(Into::into) }
        })
        .method_with::<SetConfigOption, _, _>(Access::daemon(), move |_, p| {
            let m = set_config.clone();
            async move {
                m.set_config_option(&p.session, p.config_id, p.value)
                    .await
                    .map_err(Into::into)
            }
        })
        .method_with::<SetStatus, _, _>(Access::session(|p| &p.session.0), move |_, p| {
            let r = set_status.set_status(p).map_err(Into::into);
            async move { r }
        })
        .method_with::<SetCriteria, _, _>(Access::session(|p| &p.session.0), move |cx, p| {
            let author = if matches!(*cx.scope, Scope::Daemon) {
                CriterionAuthor::User
            } else {
                CriterionAuthor::Agent
            };
            let r = set_criteria.set_criteria(p, author).map_err(Into::into);
            async move { r }
        })
        .method_with::<Rename, _, _>(Access::daemon(), move |_, p| {
            let r = rename.rename(&p.session, &p.title).map_err(Into::into);
            async move { r }
        })
        .method_with::<Close, _, _>(Access::daemon(), move |_, p| {
            let m = close.clone();
            async move { m.close(&p.session).await.map_err(Into::into) }
        })
        .method_with::<PermissionRules, _, _>(Access::daemon(), move |_, p| {
            let r = rules.rules(&p.workspace).map_err(Into::into);
            async move { r }
        })
        .method_with::<DeletePermissionRule, _, _>(Access::daemon(), move |_, p| {
            let r = delete_rule.delete_rule(p.id).map_err(Into::into);
            async move { r }
        })
        .topic_with::<Updates, _, _>(Access::daemon(), move |_, p, since| {
            let r = updates.updates(&p.session, since);
            async move { r }
        })
        .topic_with::<Changes, _, _>(Access::daemon(), move |_, p, since| {
            let r = changes.changes(p.workspace, since);
            async move { r }
        })
}
