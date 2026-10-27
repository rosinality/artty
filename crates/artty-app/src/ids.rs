//! Several servers in one app. Each numbers its windows, workspaces, panes
//! and images from 1, so the app tags every id it gets with the server's
//! number, in the top bits: ids stay unique app-wide, and the rest of the
//! app uses them as before. Messages going out lose the tag, and it says
//! which server they go to.

use artty_core::layout::Node;
use artty_core::protocol::{AppCommand, ClientMessage, Query, Reply, ServerMessage, WindowLayout};

/// A server's number: 0 is this Mac's, then the configured ones in order.
pub type ServerId = u8;

pub const LOCAL: ServerId = 0;

const SHIFT: u32 = 56;
const MASK: u64 = (1 << SHIFT) - 1;

/// `id`, from `server`, made app-wide.
pub fn tag(server: ServerId, id: u64) -> u64 {
    (u64::from(server) << SHIFT) | (id & MASK)
}

/// The server an app-wide id came from.
pub fn server_of(id: u64) -> ServerId {
    (id >> SHIFT) as ServerId
}

/// The id as its server knows it.
fn untag(id: u64) -> u64 {
    id & MASK
}

fn tag_node(server: ServerId, node: &mut Node) {
    match node {
        Node::Pane(id) => *id = tag(server, *id),
        Node::Split { first, second, .. } => {
            tag_node(server, first);
            tag_node(server, second);
        },
    }
}

fn tag_layout(server: ServerId, layout: &mut WindowLayout) {
    for workspace in &mut layout.workspaces {
        workspace.id = tag(server, workspace.id);
        for tab in &mut workspace.tabs {
            tag_node(server, &mut tab.root);
            tab.focused = tag(server, tab.focused);
        }
    }
}

/// Tag the ids in a message from `server`.
pub fn incoming(server: ServerId, message: &mut ServerMessage) {
    let tag = |id: &mut u64| *id = tag(server, *id);
    match message {
        ServerMessage::Attached { window, .. } | ServerMessage::WindowClosed { window } => {
            tag(window)
        },
        ServerMessage::Layout { window, layout } => {
            tag(window);
            tag_layout(server, layout);
        },
        ServerMessage::Frame { pane, update } => {
            tag(pane);
            for (_, line) in &mut update.lines {
                for image in &mut line.images {
                    tag(&mut image.image);
                }
            }
        },
        ServerMessage::Panes { panes } => {
            for pane in panes {
                tag(&mut pane.info.id);
            }
        },
        ServerMessage::Bell { pane } => tag(pane),
        ServerMessage::App(command) => match command {
            AppCommand::Activate { window }
            | AppCommand::OpenWindow { window }
            | AppCommand::ShowActivity { window } => tag(window),
            AppCommand::OpenSettings | AppCommand::Restart | AppCommand::OpenUrl { .. } => {},
        },
        ServerMessage::Reply { reply, .. } => match reply {
            Reply::Windows(windows) => {
                for window in windows {
                    tag(&mut window.window);
                }
            },
            Reply::Link(_)
            | Reply::Text(_)
            | Reply::Path(_)
            | Reply::Search(_)
            | Reply::Output(_)
            | Reply::Upload(_)
            | Reply::File(_)
            | Reply::Dir(_) => {},
        },
        ServerMessage::Image(image) => tag(&mut image.id),
        ServerMessage::Welcome { .. }
        | ServerMessage::Notify { .. }
        | ServerMessage::Control { .. }
        | ServerMessage::Stopping { .. }
        | ServerMessage::Sync { .. }
        | ServerMessage::Pong { .. }
        | ServerMessage::Clipboard { .. } => {},
    }
}

/// Untag the ids in a message for its server, and say which server that
/// is: the one its ids came from. `None` for a message naming nothing
/// (the sender picks the server).
pub fn outgoing(message: &mut ClientMessage) -> Option<ServerId> {
    let mut server = None;
    let mut untag = |id: &mut u64| {
        server.get_or_insert(server_of(*id));
        *id = untag(*id);
    };
    match message {
        ClientMessage::AttachWindow { window, .. } => {
            if let Some(window) = window {
                untag(window);
            }
        },
        ClientMessage::DetachWindow { window }
        | ClientMessage::WindowBounds { window, .. }
        | ClientMessage::WindowFocused { window, .. }
        | ClientMessage::NewWorkspace { window, .. }
        | ClientMessage::ToggleZoom { window }
        | ClientMessage::SetRatio { window, .. }
        | ClientMessage::EqualizePanes { window }
        | ClientMessage::ToggleBroadcast { window }
        | ClientMessage::ReopenClosedTab { window } => untag(window),
        ClientMessage::NewTab { window, workspace, .. } => {
            untag(window);
            if let Some(workspace) = workspace {
                untag(workspace);
            }
        },
        ClientMessage::Split { pane, .. }
        | ClientMessage::ClosePane { pane }
        | ClientMessage::FocusPane { pane }
        | ClientMessage::Input { pane, .. }
        | ClientMessage::Resize { pane, .. }
        | ClientMessage::Scroll { pane, .. }
        | ClientMessage::Selection { pane, .. }
        | ClientMessage::Search { pane, .. }
        | ClientMessage::EndSearch { pane }
        | ClientMessage::ClearBuffer { pane }
        | ClientMessage::JumpToPrompt { pane, .. }
        | ClientMessage::CopyMode { pane, .. }
        | ClientMessage::PaneToNewTab { pane } => untag(pane),
        ClientMessage::CloseTab { workspace, .. }
        | ClientMessage::SelectTab { workspace, .. }
        | ClientMessage::RenameTab { workspace, .. }
        | ClientMessage::RenameWorkspace { workspace, .. }
        | ClientMessage::CloseWorkspace { workspace }
        | ClientMessage::SetCollapsed { workspace, .. }
        | ClientMessage::TabToNewWorkspace { workspace, .. }
        | ClientMessage::HideWorkspace { workspace }
        | ClientMessage::WorkspaceToNewWindow { workspace }
        | ClientMessage::SetWorkspaceColor { workspace, .. }
        | ClientMessage::SetWorkspaceRoot { workspace, .. } => untag(workspace),
        ClientMessage::MoveTab { workspace, to_workspace, .. } => {
            untag(workspace);
            untag(to_workspace);
        },
        ClientMessage::MoveWorkspace { workspace, window, .. }
        | ClientMessage::OpenWorkspace { workspace, window } => {
            untag(workspace);
            untag(window);
        },
        ClientMessage::MovePane { pane, workspace, .. } => {
            untag(pane);
            untag(workspace);
        },
        ClientMessage::SwapPanes { a, b } => {
            untag(a);
            untag(b);
        },
        ClientMessage::ForgetImages { ids } => ids.iter_mut().for_each(&mut untag),
        ClientMessage::Query { query, .. } => match query {
            Query::LinkSpan { pane, .. }
            | Query::SelectionText { pane }
            | Query::Cwd { pane }
            | Query::CommandOutput { pane }
            | Query::Upload { pane, .. }
            | Query::ReadFile { pane, .. }
            | Query::ListDir { pane, .. } => untag(pane),
            Query::DetachedWindows => {},
        },
        ClientMessage::Control { .. }
        | ClientMessage::Ack { .. }
        | ClientMessage::Ping { .. }
        | ClientMessage::Appearance { .. } => {},
    }
    server
}

#[cfg(test)]
mod tests {
    use super::*;
    use artty_core::protocol::{PaneSpec, TabLayout, WorkspaceLayout};

    #[test]
    fn ids_round_trip_through_their_server() {
        let id = tag(3, 42);
        assert_ne!(id, 42);
        assert_eq!((server_of(id), untag(id)), (3, 42));
        assert_eq!(tag(LOCAL, 42), 42);

        // A layout from server 2 comes in tagged...
        let mut root = Node::Pane(5);
        root.split(5, 6, artty_core::layout::Axis::Horizontal);
        let layout = WindowLayout {
            workspaces: vec![WorkspaceLayout {
                id: 7,
                name: None,
                title: String::new(),
                color: None,
                root: None,
                tabs: vec![TabLayout {
                    title: None,
                    root,
                    focused: 6,
                    zoomed: false,
                    broadcast: false,
                }],
                active: 0,
                collapsed: false,
                cpu_percent: 0.,
                memory_bytes: 0,
            }],
            active: 0,
            hidden: 0,
        };
        let mut message = ServerMessage::Layout { window: 1, layout };
        incoming(2, &mut message);
        let ServerMessage::Layout { window, layout } = message else { unreachable!() };
        assert_eq!(window, tag(2, 1));
        let workspace = &layout.workspaces[0];
        assert_eq!(workspace.id, tag(2, 7));
        assert_eq!(workspace.tabs[0].root.panes(), vec![tag(2, 5), tag(2, 6)]);

        // ...and what's sent about it goes back to server 2, untagged.
        let mut message = ClientMessage::MovePane { pane: tag(2, 6), workspace: tag(2, 7), tab: 0 };
        assert_eq!(outgoing(&mut message), Some(2));
        assert!(matches!(message, ClientMessage::MovePane { pane: 6, workspace: 7, tab: 0 }));
        let mut message =
            ClientMessage::AttachWindow { window: None, spec: PaneSpec::default(), bounds: None };
        assert_eq!(outgoing(&mut message), None);
    }
}
