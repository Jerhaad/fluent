use super::{Checkout, FluentThread, InboundEnvelope, RunnerConfig};

pub(super) fn participant_can_contribute(
    config: &RunnerConfig,
    thread: &FluentThread,
    event: &InboundEnvelope,
) -> bool {
    match thread.project_label.as_deref() {
        Some(project_label) => config
            .projects
            .get(project_label)
            .is_some_and(|project| project.allowed_participants.contains(&event.user)),
        None => match config.channel_defaults.get(&event.channel) {
            Some(project_label) => config
                .projects
                .get(project_label)
                .is_some_and(|project| project.allowed_participants.contains(&event.user)),
            None => config
                .projects
                .values()
                .any(|project| project.allowed_participants.contains(&event.user)),
        },
    }
}

pub(super) fn available_projects(
    config: &RunnerConfig,
    participant: &str,
    checkout_is_available: impl Fn(&Checkout) -> bool,
) -> Vec<String> {
    config
        .projects
        .values()
        .filter(|project| {
            project.allowed_participants.contains(participant)
                && checkout_is_available(&project.checkout)
        })
        .map(|project| project.label.clone())
        .collect()
}

pub(super) fn actor_is_project_approver(
    config: &RunnerConfig,
    thread: &FluentThread,
    actor: &str,
) -> bool {
    thread
        .project_label
        .as_deref()
        .and_then(|label| config.projects.get(label))
        .is_some_and(|project| project.project_approvers.contains(actor))
}
