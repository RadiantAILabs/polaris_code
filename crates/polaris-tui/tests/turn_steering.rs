//! Public host contract coverage for queued and steering input.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use polaris_tui::panels::ConversationPanelPlugin;
use polaris_tui::panels::conversation::{TurnInputHandle, TurnInputMode, TurnInputReceipt};

#[derive(Default)]
struct HostQueue(Mutex<VecDeque<(TurnInputReceipt, String)>>);

impl TurnInputHandle for HostQueue {
    fn submit(&self, text: String, mode: TurnInputMode) -> Result<TurnInputReceipt, String> {
        let mut queue = self.0.lock().unwrap();
        let receipt = TurnInputReceipt {
            id: queue.len() as u64,
            target_run: queue.len() as u64 + 4,
            mode,
        };
        queue.push_back((receipt.clone(), text));
        Ok(receipt)
    }

    fn replace(&self, id: u64, text: String) -> Result<(), String> {
        let mut queue = self.0.lock().unwrap();
        let (_, body) = queue
            .iter_mut()
            .find(|(receipt, _)| receipt.id == id)
            .ok_or_else(|| "consumed".to_string())?;
        *body = text;
        Ok(())
    }

    fn remove(&self, id: u64) -> Result<(), String> {
        let mut queue = self.0.lock().unwrap();
        let index = queue
            .iter()
            .position(|(receipt, _)| receipt.id == id)
            .ok_or_else(|| "consumed".to_string())?;
        queue.remove(index);
        Ok(())
    }
}

#[test]
fn host_contract_preserves_target_and_allows_edit_or_remove_before_consumption() {
    let queue = Arc::new(HostQueue::default());
    let queued = queue.submit("first".into(), TurnInputMode::Queue).unwrap();
    let steering = queue
        .submit("redirect".into(), TurnInputMode::Steer)
        .unwrap();
    assert_eq!((queued.target_run, steering.target_run), (4, 5));
    assert_eq!(steering.mode, TurnInputMode::Steer);

    queue.replace(queued.id, "edited".into()).unwrap();
    queue.remove(steering.id).unwrap();
    let state = queue.0.lock().unwrap();
    assert_eq!(state.len(), 1);
    assert_eq!(state[0].1, "edited");
}

#[test]
fn conversation_plugin_accepts_a_host_without_owning_turn_policy() {
    let queue: Arc<dyn TurnInputHandle> = Arc::new(HostQueue::default());
    let _plugin = ConversationPanelPlugin::default().with_turn_input_handle(queue);
}
