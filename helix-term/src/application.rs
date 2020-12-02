use clap::ArgMatches as Args;
use helix_core::{indent::TAB_WIDTH, state::Mode, syntax::HighlightEvent, Position, Range, State};
use helix_view::{commands, keymap, prompt::Prompt, Editor, View};

use std::{
    borrow::Cow,
    io::{self, stdout, Stdout, Write},
    path::PathBuf,
    time::Duration,
};

use smol::prelude::*;

use anyhow::Error;

use crossterm::{
    cursor,
    cursor::position,
    event::{self, read, Event, EventStream, KeyCode, KeyEvent},
    execute, queue,
    terminal::{self, disable_raw_mode, enable_raw_mode},
};

use tui::{
    backend::CrosstermBackend,
    buffer::Buffer as Surface,
    layout::Rect,
    style::{Color, Style},
};

const OFFSET: u16 = 6; // 5 linenr + 1 gutter

type Terminal = tui::Terminal<CrosstermBackend<std::io::Stdout>>;

static EX: smol::Executor = smol::Executor::new();

const BASE_WIDTH: u16 = 30;

pub struct Application {
    editor: Editor,
    prompt: Option<Prompt>,
    terminal: Renderer,
}

struct Renderer {
    size: (u16, u16),
    terminal: Terminal,
    surface: Surface,
    cache: Surface,
    text_color: Style,
}

impl Renderer {
    pub fn new() -> Result<Self, Error> {
        let backend = CrosstermBackend::new(stdout());
        let mut terminal = Terminal::new(backend)?;
        let size = terminal::size().unwrap();
        let text_color: Style = Style::default().fg(Color::Rgb(219, 191, 239)); // lilac

        let area = Rect::new(0, 0, size.0, size.1);

        Ok(Self {
            size,
            terminal,
            surface: Surface::empty(area),
            cache: Surface::empty(area),
            text_color,
        })
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.size = (width, height);
        let area = Rect::new(0, 0, width, height);
        self.surface = Surface::empty(area);
        self.cache = Surface::empty(area);
    }

    pub fn render_view(&mut self, view: &mut View, viewport: Rect) {
        self.render_buffer(view, viewport);
        self.render_statusline(view);
    }

    // TODO: ideally not &mut View but highlights require it because of cursor cache
    pub fn render_buffer(&mut self, view: &mut View, viewport: Rect) {
        let area = Rect::new(0, 0, self.size.0, self.size.1);
        self.surface.reset(); // reset is faster than allocating new empty surface

        //  clear with background color
        self.surface
            .set_style(area, view.theme.get("ui.background"));

        // TODO: inefficient, should feed chunks.iter() to tree_sitter.parse_with(|offset, pos|)
        let source_code = view.state.doc().to_string();

        let last_line = view.last_line();

        let range = {
            // calculate viewport byte ranges
            let start = view.state.doc().line_to_byte(view.first_line);
            let end = view.state.doc().line_to_byte(last_line)
                + view.state.doc().line(last_line).len_bytes();

            start..end
        };

        // TODO: range doesn't actually restrict source, just highlight range
        // TODO: cache highlight results
        // TODO: only recalculate when state.doc is actually modified
        let highlights: Vec<_> = match view.state.syntax.as_mut() {
            Some(syntax) => {
                syntax
                    .highlight_iter(source_code.as_bytes(), Some(range), None, |_| None)
                    .unwrap()
                    .collect() // TODO: we collect here to avoid double borrow, fix later
            }
            None => vec![Ok(HighlightEvent::Source {
                start: range.start,
                end: range.end,
            })],
        };
        let mut spans = Vec::new();
        let mut visual_x = 0;
        let mut line = 0u16;
        let visible_selections: Vec<Range> = view
            .state
            .selection()
            .ranges()
            .iter()
            // TODO: limit selection to one in viewport
            // .filter(|range| !range.is_empty()) // && range.overlaps(&Range::new(start, end + 1))
            .copied()
            .collect();

        'outer: for event in highlights {
            match event.unwrap() {
                HighlightEvent::HighlightStart(span) => {
                    spans.push(span);
                }
                HighlightEvent::HighlightEnd => {
                    spans.pop();
                }
                HighlightEvent::Source { start, end } => {
                    // TODO: filter out spans out of viewport for now..

                    let start = view.state.doc().byte_to_char(start);
                    let end = view.state.doc().byte_to_char(end); // <-- index 744, len 743

                    let text = view.state.doc().slice(start..end);

                    use helix_core::graphemes::{grapheme_width, RopeGraphemes};

                    let style = match spans.first() {
                        Some(span) => view.theme.get(view.theme.scopes()[span.0].as_str()),
                        None => Style::default().fg(Color::Rgb(164, 160, 232)), // lavender
                    };

                    // TODO: we could render the text to a surface, then cache that, that
                    // way if only the selection/cursor changes we can copy from cache
                    // and paint the new cursor.

                    let mut char_index = start;

                    // iterate over range char by char
                    for grapheme in RopeGraphemes::new(&text) {
                        // TODO: track current char_index

                        if grapheme == "\n" {
                            visual_x = 0;
                            line += 1;

                            // TODO: with proper iter this shouldn't be necessary
                            if line >= viewport.height {
                                break 'outer;
                            }
                        } else if grapheme == "\t" {
                            visual_x += (TAB_WIDTH as u16);
                        } else {
                            // Cow will prevent allocations if span contained in a single slice
                            // which should really be the majority case
                            let grapheme = Cow::from(grapheme);
                            let width = grapheme_width(&grapheme) as u16;

                            // TODO: this should really happen as an after pass
                            let style = if visible_selections
                                .iter()
                                .any(|range| range.contains(char_index))
                            {
                                // cedar
                                style.clone().bg(Color::Rgb(128, 47, 0))
                            } else {
                                style
                            };

                            let style = if visible_selections
                                .iter()
                                .any(|range| range.head == char_index)
                            {
                                style.clone().bg(Color::Rgb(255, 255, 255))
                            } else {
                                style
                            };

                            // TODO: paint cursor heads except primary

                            self.surface
                                .set_string(OFFSET + visual_x, line, grapheme, style);

                            visual_x += width;
                        }
                        // if grapheme == "\t"

                        char_index += 1;
                    }
                }
            }
        }
        let style: Style = view.theme.get("ui.linenr");
        let last_line = view.last_line();
        for (i, line) in (view.first_line..last_line).enumerate() {
            self.surface
                .set_stringn(0, i as u16, format!("{:>5}", line + 1), 5, style);
        }
    }

    pub fn render_statusline(&mut self, view: &View) {
        let mode = match view.state.mode() {
            Mode::Insert => "INS",
            Mode::Normal => "NOR",
            Mode::Goto => "GOTO",
        };
        // statusline
        self.surface.set_style(
            Rect::new(0, self.size.1 - 2, self.size.0, 1),
            view.theme.get("ui.statusline"),
        );
        self.surface
            .set_string(1, self.size.1 - 2, mode, self.text_color);
    }

    pub fn render_prompt(&mut self, view: &View, prompt: &Prompt) {
        // completion
        if !prompt.completion.is_empty() {
            // TODO: find out better way of clearing individual lines of the screen
            let mut row = 0;
            let mut col = 0;
            let max_col = self.size.0 / BASE_WIDTH;
            let col_height = ((prompt.completion.len() as u16 + max_col - 1) / max_col);

            for i in (3..col_height + 3) {
                self.surface.set_string(
                    0,
                    self.size.1 - i as u16,
                    " ".repeat(self.size.0 as usize),
                    self.text_color,
                );
            }
            self.surface.set_style(
                Rect::new(0, self.size.1 - col_height - 2, self.size.0, col_height),
                view.theme.get("ui.statusline"),
            );
            for (i, command) in prompt.completion.iter().enumerate() {
                let color = if prompt.completion_selection_index.is_some()
                    && i == prompt.completion_selection_index.unwrap()
                {
                    Style::default().bg(Color::Rgb(104, 060, 232))
                } else {
                    self.text_color
                };
                self.surface.set_stringn(
                    1 + col * BASE_WIDTH,
                    self.size.1 - col_height - 2 + row,
                    &command,
                    BASE_WIDTH as usize - 1,
                    color,
                );
                row += 1;
                if row > col_height - 1 {
                    row = 0;
                    col += 1;
                }
                if col > max_col {
                    break;
                }
            }
        }
        // render buffer text
        self.surface
            .set_string(1, self.size.1 - 1, &prompt.prompt, self.text_color);
        self.surface
            .set_string(2, self.size.1 - 1, &prompt.line, self.text_color);
    }

    pub fn draw(&mut self) {
        use tui::backend::Backend;
        // TODO: theres probably a better place for this
        self.terminal
            .backend_mut()
            .draw(self.cache.diff(&self.surface).into_iter());
        // swap the buffer
        std::mem::swap(&mut self.surface, &mut self.cache);
    }

    pub fn render_cursor(&mut self, view: &View, prompt: Option<&Prompt>, viewport: Rect) {
        let mut stdout = stdout();
        match view.state.mode() {
            Mode::Insert => write!(stdout, "\x1B[6 q"),
            mode => write!(stdout, "\x1B[2 q"),
        };
        let pos = if let Some(prompt) = prompt {
            Position::new(self.size.0 as usize, 2 + prompt.cursor)
        } else {
            if let Some(path) = view.state.path() {
                self.surface.set_string(
                    6,
                    self.size.1 - 1,
                    path.to_string_lossy(),
                    self.text_color,
                );
            }

            let cursor = view.state.selection().cursor();

            let mut pos = view
                .screen_coords_at_pos(&view.state.doc().slice(..), cursor)
                .expect("Cursor is out of bounds.");
            pos.col += viewport.x as usize;
            pos.row += viewport.y as usize;
            pos
        };

        execute!(stdout, cursor::MoveTo(pos.col as u16, pos.row as u16));
    }
}

impl Application {
    pub fn new(mut args: Args) -> Result<Self, Error> {
        let terminal = Renderer::new()?;
        let mut editor = Editor::new();

        if let Some(file) = args.values_of_t::<PathBuf>("files").unwrap().pop() {
            editor.open(file, terminal.size)?;
        }

        let mut app = Self {
            editor,
            terminal,
            // TODO; move to state
            prompt: None,
        };

        Ok(app)
    }

    fn render(&mut self) {
        let viewport = Rect::new(OFFSET, 0, self.terminal.size.0, self.terminal.size.1 - 2); // - 2 for statusline and prompt

        if let Some(view) = &mut self.editor.view {
            self.terminal.render_view(view, viewport);
            if let Some(prompt) = &self.prompt {
                if prompt.should_close {
                    self.prompt = None;
                } else {
                    self.terminal.render_prompt(view, prompt);
                }
            }
        }

        self.terminal.draw();

        // TODO: drop unwrap
        self.terminal.render_cursor(
            self.editor.view.as_ref().unwrap(),
            self.prompt.as_ref(),
            viewport,
        );
    }

    pub async fn event_loop(&mut self) {
        let mut reader = EventStream::new();
        let keymap = keymap::default();

        self.render();

        loop {
            if self.editor.should_close {
                break;
            }

            // Handle key events
            match reader.next().await {
                Some(Ok(Event::Resize(width, height))) => {
                    self.terminal.resize(width, height);

                    // TODO: simplistic ensure cursor in view for now
                    if let Some(view) = &mut self.editor.view {
                        view.size = self.terminal.size;
                        view.ensure_cursor_in_view()
                    };

                    self.render();
                }
                Some(Ok(Event::Key(event))) => {
                    // if there's a prompt, it takes priority
                    if let Some(prompt) = &mut self.prompt {
                        self.prompt
                            .as_mut()
                            .unwrap()
                            .handle_input(event, &mut self.editor);

                        self.render();
                    } else if let Some(view) = &mut self.editor.view {
                        let keys = vec![event];
                        // TODO: sequences (`gg`)
                        // TODO: handle count other than 1
                        match view.state.mode() {
                            Mode::Insert => {
                                if let Some(command) = keymap[&Mode::Insert].get(&keys) {
                                    command(view, 1);
                                } else if let KeyEvent {
                                    code: KeyCode::Char(c),
                                    ..
                                } = event
                                {
                                    commands::insert::insert_char(view, c);
                                }
                                view.ensure_cursor_in_view();
                            }
                            Mode::Normal => {
                                if let &[KeyEvent {
                                    code: KeyCode::Char(':'),
                                    ..
                                }] = keys.as_slice()
                                {
                                    let prompt = Prompt::new(
                                        ":".to_owned(),
                                        |_input: &str| {
                                            // TODO: i need this duplicate list right now to avoid borrow checker issues
                                            let command_list = vec![
                                                String::from("q"),
                                                String::from("aaa"),
                                                String::from("bbb"),
                                                String::from("ccc"),
                                                String::from("ddd"),
                                                String::from("eee"),
                                                String::from("averylongcommandaverylongcommandaverylongcommandaverylongcommandaverylongcommand"),
                                                String::from("q"),
                                                String::from("aaa"),
                                                String::from("bbb"),
                                                String::from("ccc"),
                                                String::from("ddd"),
                                                String::from("eee"),
                                                String::from("q"),
                                                String::from("aaa"),
                                                String::from("bbb"),
                                                String::from("ccc"),
                                                String::from("ddd"),
                                                String::from("eee"),
                                                String::from("q"),
                                                String::from("aaa"),
                                                String::from("bbb"),
                                                String::from("ccc"),
                                                String::from("ddd"),
                                                String::from("eee"),
                                                String::from("q"),
                                                String::from("aaa"),
                                                String::from("bbb"),
                                                String::from("ccc"),
                                                String::from("ddd"),
                                                String::from("eee"),
                                            ];
                                            command_list
                                                .into_iter()
                                                .filter(|command| command.contains(_input))
                                                .collect()
                                        }, // completion
                                        |editor: &mut Editor, input: &str| match input {
                                            "q" => editor.should_close = true,
                                            _ => (),
                                        },
                                    );

                                    self.prompt = Some(prompt);

                                // HAXX: special casing for command mode
                                } else if let Some(command) = keymap[&Mode::Normal].get(&keys) {
                                    command(view, 1);

                                    // TODO: simplistic ensure cursor in view for now
                                    view.ensure_cursor_in_view();
                                }
                            }
                            mode => {
                                if let Some(command) = keymap[&mode].get(&keys) {
                                    command(view, 1);

                                    // TODO: simplistic ensure cursor in view for now
                                    view.ensure_cursor_in_view();
                                }
                            }
                        }
                        self.render();
                    }
                }
                Some(Ok(Event::Mouse(_))) => (), // unhandled
                Some(Err(x)) => panic!(x),
                None => break,
            }
        }
    }

    pub async fn run(&mut self) -> Result<(), Error> {
        enable_raw_mode()?;

        let mut stdout = stdout();

        execute!(stdout, terminal::EnterAlternateScreen)?;

        // Exit the alternate screen and disable raw mode before panicking
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            execute!(std::io::stdout(), terminal::LeaveAlternateScreen);
            disable_raw_mode();
            hook(info);
        }));

        self.event_loop().await;

        // reset cursor shape
        write!(stdout, "\x1B[2 q");

        execute!(stdout, terminal::LeaveAlternateScreen)?;

        disable_raw_mode()?;

        Ok(())
    }
}

// TODO: language configs:
// tabSize, fileExtension etc, mapping to tree sitter parser
// themes:
// map tree sitter highlights to color values
//
// TODO: expand highlight thing so we're able to render only viewport range
// TODO: async: maybe pre-cache scopes as empty so we render all graphemes initially as regular
////text until calc finishes
// TODO: scope matching: biggest union match? [string] & [html, string], [string, html] & [ string, html]
// can do this by sorting our theme matches based on array len (longest first) then stopping at the
// first rule that matches (rule.all(|scope| scopes.contains(scope)))
