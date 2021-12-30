// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{bail, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use hif::*;
use humility::core::Core;
use humility::hubris::*;
use humility_cmd::hiffy::*;
use humility_cmd::idol;
use humility_cmd::{Archive, Args, Attach, Command, Validate};
use std::io;
use std::time::{Duration, Instant};
use structopt::clap::App;
use structopt::StructOpt;
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    symbols,
    text::{Span, Spans},
    widgets::{
        Axis, Block, Borders, Chart, Dataset, List, ListItem, ListState,
    },
    Frame, Terminal,
};

#[derive(StructOpt, Debug)]
#[structopt(name = "dashboard", about = env!("CARGO_PKG_DESCRIPTION"))]
struct DashboardArgs {
    /// sets timeout
    #[structopt(
        long, short = "T", default_value = "5000", value_name = "timeout_ms",
        parse(try_from_str = parse_int::parse)
    )]
    timeout: u32,
}

struct StatefulList {
    state: ListState,
    n: usize,
}

impl StatefulList {
    fn next(&mut self) {
        self.state.select(match self.state.selected() {
            Some(ndx) => Some((ndx + 1) % self.n),
            None => Some(0),
        });
    }

    fn previous(&mut self) {
        self.state.select(match self.state.selected() {
            Some(ndx) if ndx == 0 => Some(self.n - 1),
            Some(ndx) => Some(ndx - 1),
            None => Some(0),
        });
    }

    fn unselect(&mut self) {
        self.state.select(None);
    }
}

struct Series {
    name: String,
    color: Color,
    data: Vec<(f64, f64)>,
    raw: Vec<Option<f32>>,
}

struct Dashboard<'a> {
    context: HiffyContext<'a>,
    ops: Vec<Op>,
    last: Instant,
    interval: u32,
    outstanding: bool,
    series: Vec<Series>,
    legend: StatefulList,
    time: usize,
    width: usize,
    bounds: [f64; 2],
}

impl<'a> Dashboard<'a> {
    fn new(
        hubris: &'a HubrisArchive,
        core: &mut dyn Core,
        subargs: &DashboardArgs,
    ) -> Result<Dashboard<'a>> {
        let mut context = HiffyContext::new(hubris, core, subargs.timeout)?;

        let ops = thermal_ops(hubris, &mut context)?;

        context.start(core, ops.as_slice(), None)?;

        let mut series = vec![];

        let all = ["NW", "N", "NE", "SE", "S", "SW", "CPU"];

        let colors = [
            Color::LightMagenta,
            Color::LightYellow,
            Color::LightCyan,
            Color::LightGreen,
            Color::LightBlue,
            Color::LightRed,
            Color::Cyan,
            Color::Green,
            Color::Blue,
            Color::Red,
        ];

        for (ndx, s) in all.iter().enumerate() {
            series.push(Series {
                name: s.to_string(),
                color: colors[ndx % colors.len()],
                data: Vec::new(),
                raw: Vec::new(),
            })
        }

        Ok(Dashboard {
            context,
            ops,
            outstanding: true,
            last: Instant::now(),
            interval: 1000,
            series,
            legend: StatefulList { state: ListState::default(), n: all.len() },
            time: 0,
            width: 600,
            bounds: [20.0, 120.0],
        })
    }

    fn need_update(&mut self, core: &mut dyn Core) -> Result<bool> {
        if self.outstanding {
            if self.context.done(core)? {
                let results = self.context.results(core)?;

                for (ndx, r) in results.iter().enumerate() {
                    self.series[ndx].raw.push(if let Ok(val) = r {
                        Some(f32::from_le_bytes(val[0..4].try_into()?))
                    } else {
                        None
                    })
                }

                self.time += 1;
                self.outstanding = false;
                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            if self.last.elapsed().as_millis() > self.interval.into() {
                self.context.start(core, self.ops.as_slice(), None)?;
                self.last = Instant::now();
                self.outstanding = true;
            }

            Ok(false)
        }
    }

    fn update_data(&mut self) {
        for s in &mut self.series {
            s.data = Vec::new();
        }

        for i in 0..self.width {
            if self.time < self.width - i {
                continue;
            }

            let offs = (self.time - (self.width - i)) as usize;

            for (_ndx, s) in &mut self.series.iter_mut().enumerate() {
                if let Some(datum) = s.raw[offs] {
                    s.data.push((i as f64, datum as f64));
                }
            }
        }

        self.update_bounds();
    }

    fn update_bounds(&mut self) {
        let selected = self.legend.state.selected();
        let mut min = None;
        let mut max = None;

        for (ndx, s) in self.series.iter().enumerate() {
            if let Some(selected) = selected {
                if ndx != selected {
                    continue;
                }
            }

            for (_, datum) in &s.data {
                min = match min {
                    Some(min) if datum < min => Some(datum),
                    None => Some(datum),
                    _ => min,
                };

                max = match max {
                    Some(max) if datum > max => Some(datum),
                    None => Some(datum),
                    _ => max,
                };
            }
        }

        if let Some(min) = min {
            self.bounds[0] = ((min * 0.85) / 2.0) * 2.0;
        }

        if let Some(max) = max {
            self.bounds[1] = ((max * 1.15) / 2.0) * 2.0;
        }
    }

    fn up(&mut self) {
        self.legend.previous();
    }

    fn down(&mut self) {
        self.legend.next();
    }

    fn esc(&mut self) {
        self.legend.unselect();
    }

    fn enter(&mut self) {}
}

fn run_dashboard<B: Backend>(
    terminal: &mut Terminal<B>,
    mut dashboard: Dashboard,
    core: &mut dyn Core,
) -> Result<()> {
    let mut last_tick = Instant::now();
    let tick_rate = Duration::from_millis(100);

    loop {
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        let update = if crossterm::event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Up => dashboard.up(),
                    KeyCode::Down => dashboard.down(),
                    KeyCode::Esc => dashboard.esc(),
                    KeyCode::Enter => dashboard.enter(),
                    _ => {}
                }
            }
            true
        } else {
            dashboard.need_update(core)?
        };

        if update {
            dashboard.update_data();
            terminal.draw(|f| draw(f, &mut dashboard))?;
        }

        last_tick = Instant::now();
    }
}

fn dashboard(
    hubris: &mut HubrisArchive,
    core: &mut dyn Core,
    _args: &Args,
    subargs: &[String],
) -> Result<()> {
    let subargs = DashboardArgs::from_iter_safe(subargs)?;
    let dashboard = Dashboard::new(hubris, core, &subargs)?;

    // setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_dashboard(&mut terminal, dashboard, core);

    // restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    res?;

    Ok(())
}

pub fn init<'a, 'b>() -> (Command, App<'a, 'b>) {
    (
        Command::Attached {
            name: "dashboard",
            archive: Archive::Required,
            attach: Attach::LiveOnly,
            validate: Validate::Booted,
            run: dashboard,
        },
        DashboardArgs::clap(),
    )
}

fn thermal_ops(
    hubris: &HubrisArchive,
    context: &mut HiffyContext,
) -> Result<Vec<Op>> {
    let mut ops = vec![];
    let funcs = context.functions()?;
    let op = idol::IdolOperation::new(hubris, "Thermal", "read_sensor", None)?;

    let ok = hubris.lookup_basetype(op.ok)?;

    if ok.encoding != HubrisEncoding::Float {
        bail!("expected return value of read_sensor() to be a float");
    }

    if ok.size != 4 {
        bail!("expected return value of read_sensor() to be an f32");
    }

    for i in 0..7 {
        let payload =
            op.payload(&[("index", idol::IdolArgument::Scalar(i))])?;
        context.idol_call_ops(&funcs, &op, &payload, &mut ops)?;
    }

    ops.push(Op::Done);

    Ok(ops)
}

fn draw<B: Backend>(f: &mut Frame<B>, dashboard: &mut Dashboard) {
    let size = f.size();
    let screen = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
            ]
            .as_ref(),
        )
        .split(size);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            [Constraint::Ratio(4, 5), Constraint::Ratio(1, 5)].as_ref(),
        )
        .split(screen[0]);

    let x_labels = vec![
        Span::styled(
            format!("t-{}", dashboard.width),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("t-{}", 1),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];

    let mut datasets = vec![];
    let selected = dashboard.legend.state.selected();

    for (ndx, s) in dashboard.series.iter().enumerate() {
        if let Some(selected) = selected {
            if ndx != selected {
                continue;
            }
        }

        datasets.push(
            Dataset::default()
                .name(&s.name)
                .marker(symbols::Marker::Braille)
                .style(
                    Style::default().fg(s.color).add_modifier(Modifier::BOLD),
                )
                .data(&s.data),
        );
    }

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(Span::styled(
                    "Temperature",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL),
        )
        .x_axis(
            Axis::default()
                .title("Time")
                .style(Style::default().fg(Color::Gray))
                .labels(x_labels)
                .bounds([0.0, dashboard.width as f64]),
        )
        .y_axis(
            Axis::default()
                .title("Degrees Celsius")
                .style(Style::default().fg(Color::Gray))
                .labels(vec![
                    Span::styled(
                        format!("{:2.0}°", dashboard.bounds[0]),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:2.0}°", dashboard.bounds[1]),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ])
                .bounds(dashboard.bounds),
        );

    f.render_widget(chart, chunks[0]);

    let mut rows = vec![];

    for s in &dashboard.series {
        let val = if !s.data.is_empty() {
            format!("{:2.0}°", s.data[s.data.len() - 1].1)
        } else {
            "-".to_string()
        };

        rows.push(ListItem::new(Spans::from(vec![
            Span::styled(
                format!("{:<10}", s.name),
                Style::default().fg(s.color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                val,
                Style::default().fg(s.color).add_modifier(Modifier::BOLD),
            ),
        ])));
    }

    let list = List::new(rows)
        .block(Block::default().borders(Borders::ALL).title("List"))
        .highlight_style(
            Style::default()
                .bg(Color::LightGreen)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        );

    //        .highlight_symbol(">> ");

    // We can now render the item list
    f.render_stateful_widget(list, chunks[1], &mut dashboard.legend.state);

    /*
    let table = Table::new(rows)
        .header(
            Row::new(vec!["Sensor", "Temp"])
                .style(Style::default().fg(Color::Gray))
        )
        .block(
            Block::default()
                .title(Span::styled(
                    "Sensors",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL),
        )
        .widths(&[
            Constraint::Ratio(1, 2),
            Constraint::Ratio(1, 2),
        ]);

    f.render_widget(table, chunks[1]);
    */
}
