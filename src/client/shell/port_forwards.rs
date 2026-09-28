//! Sidebar section that lists ports forwarded from saved machines.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::render::put_text;
use super::*;
use crate::client::port_forward::{PortForwardState, PortForwardView};

/// Rows kept for the agent panel above the ports section.
const MIN_AGENT_ROWS: u16 = 3;
const MAX_PORT_ROWS: usize = 4;

/// Returns the rows of `area` that the ports section uses, taken from the bottom.
pub(super) fn section_height(area: Rect, forwards: &[PortForwardView]) -> u16 {
    if forwards.is_empty() {
        return 0;
    }
    let rows = if forwards.len() > MAX_PORT_ROWS {
        MAX_PORT_ROWS + 1
    } else {
        forwards.len()
    };
    let height = 2 + rows as u16;
    if area.height < height + MIN_AGENT_ROWS {
        return 0;
    }
    height
}

pub(super) fn row_label(forward: &PortForwardView) -> String {
    let marker = match forward.state {
        PortForwardState::Failed => "!",
        PortForwardState::Active | PortForwardState::Shared => " ",
    };
    if forward.local_port == forward.remote_port {
        format!("{marker}:{} ← {}", forward.local_port, forward.machine)
    } else {
        format!(
            "{marker}:{} ← {}:{}",
            forward.local_port, forward.machine, forward.remote_port
        )
    }
}

pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    forwards: &[PortForwardView],
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    if area.height < 3 || forwards.is_empty() {
        return;
    }
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(area.width as usize),
        Style::default().fg(palette.surface_dim),
    );
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        " ports",
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    // The sidebar toggle occupies the right edge of the last row.
    let width = area.width.saturating_sub(2);
    let visible = if forwards.len() > MAX_PORT_ROWS {
        MAX_PORT_ROWS
    } else {
        forwards.len()
    };
    let mut y = area.y + 2;
    for forward in &forwards[..visible] {
        if y >= area.bottom() {
            return;
        }
        let style = match forward.state {
            PortForwardState::Active => Style::default().fg(palette.text),
            PortForwardState::Shared => Style::default().fg(palette.overlay0),
            PortForwardState::Failed => Style::default().fg(palette.red),
        };
        put_text(buffer, area.x, y, width, &row_label(forward), style);
        let rect = Rect::new(area.x, y, width, 1);
        hits.port_forwards.push((rect, forward.local_port));
        y += 1;
    }
    if forwards.len() > visible && y < area.bottom() {
        put_text(
            buffer,
            area.x,
            y,
            width,
            &format!("  +{} more", forwards.len() - visible),
            Style::default().fg(palette.overlay0),
        );
    }
}

impl ClientShellState {
    pub(crate) fn set_port_forwards(&mut self, forwards: Vec<PortForwardView>) -> bool {
        if self.port_forwards == forwards {
            return false;
        }
        self.port_forwards = forwards;
        true
    }

    /// Opens the local address of a clicked forward in the browser.
    pub(super) fn handle_port_forward_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(local_port) = self
            .hits
            .port_forwards
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, local_port)| *local_port)
        else {
            return false;
        };
        outcome
            .actions
            .push(ClientShellAction::OpenSafeWebUrl(format!(
                "http://localhost:{local_port}/"
            )));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forward(local_port: u16, remote_port: u16, state: PortForwardState) -> PortForwardView {
        PortForwardView {
            machine: "workbox".into(),
            remote_port,
            local_port,
            state,
        }
    }

    #[test]
    fn labels_show_the_remote_port_only_when_it_differs() {
        assert_eq!(
            row_label(&forward(5173, 5173, PortForwardState::Active)),
            " :5173 ← workbox"
        );
        assert_eq!(
            row_label(&forward(5174, 5173, PortForwardState::Active)),
            " :5174 ← workbox:5173"
        );
        assert_eq!(
            row_label(&forward(3000, 3000, PortForwardState::Failed)),
            "!:3000 ← workbox"
        );
    }

    #[test]
    fn the_section_yields_to_a_short_agent_panel() {
        let forwards = vec![forward(3000, 3000, PortForwardState::Active)];
        assert_eq!(section_height(Rect::new(0, 0, 30, 20), &[]), 0);
        assert_eq!(section_height(Rect::new(0, 0, 30, 20), &forwards), 3);
        assert_eq!(section_height(Rect::new(0, 0, 30, 5), &forwards), 0);
        let many = (0..10)
            .map(|port| forward(3000 + port, 3000 + port, PortForwardState::Active))
            .collect::<Vec<_>>();
        assert_eq!(
            section_height(Rect::new(0, 0, 30, 20), &many),
            2 + MAX_PORT_ROWS as u16 + 1
        );
    }

    #[test]
    fn rows_render_and_register_click_targets() {
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let forwards = vec![
            forward(5173, 5173, PortForwardState::Active),
            forward(8001, 8000, PortForwardState::Shared),
        ];
        let area = Rect::new(0, 0, 30, 4);
        let mut buffer = Buffer::empty(area);
        let mut hits = ShellHitMap::default();
        render(&mut buffer, area, &forwards, &config, &mut hits);
        let row = |y: u16| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        assert!(row(1).starts_with(" ports"));
        assert!(row(2).starts_with(" :5173 ← workbox"));
        assert!(row(3).starts_with(" :8001 ← workbox:8000"));
        assert_eq!(
            hits.port_forwards,
            vec![
                (Rect::new(0, 2, 28, 1), 5173),
                (Rect::new(0, 3, 28, 1), 8001)
            ]
        );
    }
}
