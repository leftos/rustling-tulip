//! Agent options specs: the daemon's agent descriptors and each agent's
//! Settings values, as the client keeps them for the spawn dialog.

use crate::support;

use std::collections::BTreeMap;

use gpui::TestAppContext;
use protocol::{AgentId, DaemonMessage, OptionValue, OptionValues};
use support::{Harness, TestDir};

#[gpui::test]
fn the_client_keeps_agents_and_settings_until_the_connection_drops(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    assert_eq!(
        h.root(|root, _| root.agents().to_vec()),
        agent_registry::descriptors(),
        "the descriptors the daemon sends after Welcome"
    );
    assert!(
        h.root(|root, _| root.agent_settings().is_empty()),
        "no agent has Settings values yet"
    );

    let settings = BTreeMap::from([(
        AgentId("claude".to_owned()),
        OptionValues::from([(
            "provider".to_owned(),
            OptionValue::Text("deepseek".to_owned()),
        )]),
    )]);
    h.send(DaemonMessage::AgentSettings {
        settings: settings.clone(),
    });
    assert_eq!(
        h.root(|root, _| root.agent_settings().clone()),
        settings,
        "a Settings broadcast replaces the values"
    );

    h.lose_connection();
    assert!(
        h.root(|root, _| root.agents().is_empty()),
        "a dropped connection forgets the agents"
    );
    assert!(
        h.root(|root, _| root.agent_settings().is_empty()),
        "a dropped connection forgets the Settings values"
    );
}
