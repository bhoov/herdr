use super::HeadlessServer;

impl HeadlessServer {
    /// Records a shell's port subscription and sends it the current list. When probing was
    /// paused because nothing subscribed, the list is sent after a verifying probe instead.
    pub(super) fn subscribe_port_announcements(&mut self, client_id: u64) {
        if !self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.is_shell_client())
        {
            return;
        }
        let resumed = !self.has_port_subscriber()
            && self.app.announced_ports.resume(std::time::Instant::now());
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        client.shell_port_announcements = true;
        client.shell_port_announcements_pending = resumed;
        if !resumed {
            if let Some(message) = self.port_announcements_message() {
                self.send_to_client(client_id, message);
            }
        }
    }

    pub(super) fn has_pending_port_subscriber(&self) -> bool {
        self.clients
            .values()
            .any(|client| client.shell_port_announcements_pending)
    }

    pub(super) fn has_port_subscriber(&self) -> bool {
        self.clients
            .values()
            .any(|client| client.shell_port_announcements)
    }

    /// Sends the listening announced ports to every subscribed shell.
    pub(super) fn broadcast_port_announcements(&mut self) {
        let client_ids = self
            .clients
            .iter()
            .filter(|(_, client)| client.shell_port_announcements)
            .map(|(client_id, _)| *client_id)
            .collect::<Vec<_>>();
        if client_ids.is_empty() {
            return;
        }
        let Some(message) = self.port_announcements_message() else {
            return;
        };
        for client_id in client_ids {
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.shell_port_announcements_pending = false;
            }
            self.send_to_client(client_id, message.clone());
        }
    }

    fn port_announcements_message(&self) -> Option<crate::protocol::ServerMessage> {
        let announcements = self.app.port_announcements(&self.client_shell_boot_id);
        crate::protocol::endpoint::port_announcements_message(&announcements)
            .inspect_err(|err| tracing::warn!(err = %err, "failed to encode port announcements"))
            .ok()
    }
}
