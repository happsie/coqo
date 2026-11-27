// SPDX-License-Identifier: Apache-2.0

//! The protocol routes of `coqo.agents@1` (`agents/*`, `sessions/*`).

use coqo_protocol::Router;
use coqo_protocol::methods::agents::{
    Cancel, Changes, Close, Create, Get, List, Prompt, Providers, RespondPermission, SetMode,
    Updates,
};

use crate::manager::SessionManager;

/// Adds the agent methods and topics, served by `manager`.
pub fn register(router: Router, manager: SessionManager) -> Router {
    let m = manager;
    let providers = m.clone();
    let list = m.clone();
    let get = m.clone();
    let create = m.clone();
    let prompt = m.clone();
    let cancel = m.clone();
    let respond = m.clone();
    let set_mode = m.clone();
    let close = m.clone();
    let updates = m.clone();
    let changes = m;
    router
        .method::<Providers, _, _>(move |_, ()| {
            let r = providers.providers();
            async move { Ok(r) }
        })
        .method::<List, _, _>(move |_, p| {
            let r = list.list(&p).map_err(Into::into);
            async move { r }
        })
        .method::<Get, _, _>(move |_, p| {
            let r = get.snapshot(&p.session).map_err(Into::into);
            async move { r }
        })
        .method::<Create, _, _>(move |_, p| {
            let m = create.clone();
            async move { m.create(p).await.map_err(Into::into) }
        })
        .method::<Prompt, _, _>(move |_, p| {
            let m = prompt.clone();
            async move { m.prompt(&p.session, p.content).await.map_err(Into::into) }
        })
        .method::<Cancel, _, _>(move |_, p| {
            let m = cancel.clone();
            async move { m.cancel(&p.session).await.map_err(Into::into) }
        })
        .method::<RespondPermission, _, _>(move |_, p| {
            let r = respond
                .respond_permission(&p.session, p.request_id, p.outcome)
                .map_err(Into::into);
            async move { r }
        })
        .method::<SetMode, _, _>(move |_, p| {
            let m = set_mode.clone();
            async move { m.set_mode(&p.session, p.mode_id).await.map_err(Into::into) }
        })
        .method::<Close, _, _>(move |_, p| {
            let m = close.clone();
            async move { m.close(&p.session).await.map_err(Into::into) }
        })
        .topic::<Updates, _, _>(move |_, p, since| {
            let r = updates.updates(&p.session, since);
            async move { r }
        })
        .topic::<Changes, _, _>(move |_, p, since| {
            let r = changes.changes(p.workspace, since);
            async move { r }
        })
}
