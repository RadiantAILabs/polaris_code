//! Settle calls when cancellation stops the executor between graph nodes.

use super::*;
use std::sync::{Mutex, OnceLock};

use polaris::graph::MiddlewareAPI;
use polaris::graph::hooks::{HooksAPI, api::BoxedHook, schedule::OnGraphFailure};
use polaris::plugins::{IOError, IOProvider};
use polaris::system::plugin::Schedule;

use crate::terminal::transcript::tool_message;

struct SharedIO(OnceLock<UserIO>);
impl IOProvider for SharedIO {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        self.0.get().ok_or(IOError::Closed)?.send(message).await
    }
    async fn receive(&self) -> Result<IOMessage, IOError> {
        self.0.get().ok_or(IOError::Closed)?.receive().await
    }
    async fn close(&self) {
        if let Some(io) = self.0.get() {
            io.close().await;
        }
    }
}

#[derive(Clone)]
pub(crate) struct ToolCleanup {
    io: Arc<SharedIO>,
    messages: Arc<Mutex<Vec<IOMessage>>>,
}
impl LocalResource for ToolCleanup {}

pub(crate) fn setup_cleanup(ctx: &mut SystemContext<'_>) {
    if ctx.get_resource::<UserIO>().is_err() {
        return;
    }
    let io = Arc::new(SharedIO(OnceLock::new()));
    // Setup owns the context exclusively. Fill the forwarding channel before
    // any system can use it; every receive still reaches the original provider.
    let type_id = std::any::TypeId::of::<UserIO>();
    let original = match ctx.factory_fn_by_type_id(type_id) {
        // Topology runs forward a fresh channel from this factory. Keep it
        // attached when replacing the session's own channel with the wrapper.
        Some(factory) => ctx
            .insert_boxed_with_factory(type_id, Box::new(UserIO::new(io.clone())), factory)
            .and_then(|original| original.downcast::<UserIO>().ok())
            .map(|original| *original),
        None => ctx.insert(UserIO::new(io.clone())),
    }
    .expect("the session has UserIO");
    assert!(
        io.0.set(original).is_ok(),
        "the forwarding channel is bound once"
    );
    ctx.insert(ToolCleanup {
        io,
        messages: Arc::new(Mutex::new(Vec::new())),
    });
}

pub(crate) fn install_cleanup(server: &mut Server) {
    if !server.contains_api::<HooksAPI>() {
        server.insert_api(HooksAPI::new());
    }
    if !server.contains_api::<MiddlewareAPI>() {
        server.insert_api(MiddlewareAPI::new());
    }
    // Failure hooks can update canonical resources after graph borrows end.
    // The surrounding middleware then delivers those closures asynchronously.
    server
        .api::<HooksAPI>()
        .expect("hooks API was installed")
        .register_boxed(
            OnGraphFailure::schedule_id(),
            "polaris_code::tool_batch_cleanup",
            BoxedHook::new(|ctx, _| close_interrupted_batch(ctx), Vec::new()),
        )
        .expect("the tool cleanup hook is registered once");
    server
        .api::<MiddlewareAPI>()
        .expect("middleware API was installed")
        .register_graph_execution("polaris_code::tool_batch_cleanup", |_info, ctx, next| {
            let cleanup = ctx
                .get_resource::<ToolCleanup>()
                .ok()
                .map(|cleanup| ToolCleanup::clone(&cleanup));
            Box::pin(async move {
                let result = next.run(ctx).await;
                if let Some(cleanup) = cleanup {
                    let messages = std::mem::take(
                        &mut *cleanup
                            .messages
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()),
                    );
                    for message in messages {
                        let _ = cleanup.io.send(message).await;
                    }
                }
                result
            })
        });
}

fn close_interrupted_batch(ctx: &mut SystemContext<'_>) {
    let Ok(cleanup) = ctx
        .get_resource::<ToolCleanup>()
        .map(|cleanup| ToolCleanup::clone(&cleanup))
    else {
        return;
    };
    let Ok(store) = ctx
        .get_resource::<OxigraphStore>()
        .map(|store| OxigraphStore::clone(&store))
    else {
        return;
    };
    let Ok(mut batch) = ctx
        .get_resource_mut::<ToolBatch>()
        .map(|mut batch| std::mem::take(&mut *batch))
    else {
        return;
    };
    let Ok(lifetime) = ctx
        .get_resource_mut::<CallLifetime>()
        .map(|mut lifetime| std::mem::take(&mut *lifetime))
    else {
        return;
    };
    let Ok(mut context) = ctx.get_resource_mut::<ContextManager>() else {
        return;
    };
    let mut messages = cleanup
        .messages
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let CallLifetime::Active(active) = lifetime {
        let reason = "Interrupted by user";
        messages.push(active.activity.finish_message(PHASE_ERROR, reason));
        record_tool_call(
            &store,
            context.turn_marker(),
            &active.call.function.name,
            &active.call.function.arguments,
            "interrupted",
            reason,
        );
        batch.results.push(with_approval_note(
            active.call.error(ToolResultContent::Text(reason.into())),
            active.approval_note.as_deref(),
        ));
    }
    for (_, call) in batch.calls.drain(..) {
        let name = &call.function.name;
        let reason = "not run: turn interrupted";
        messages.push(tool_message(
            &call.id,
            None,
            name,
            PHASE_START,
            0,
            &tool_detail(name, &call.function.arguments),
        ));
        messages.push(tool_message(&call.id, None, name, PHASE_ERROR, 0, reason));
        record_tool_call(
            &store,
            context.turn_marker(),
            name,
            &call.function.arguments,
            "interrupted",
            reason,
        );
        batch
            .results
            .push(call.error(ToolResultContent::Text(reason.into())));
    }
    if !batch.results.is_empty() {
        context.push(Message::User {
            content: batch.results,
        });
    }
}
