use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    symbols,
    widgets::{Block, Borders, Chart, Dataset, Paragraph},
    Terminal,
};
use rodio::{OutputStream, OutputStreamHandle, Sink, Source};
use serde::Deserialize;
use std::{
    f32::consts::PI,
    fs,
    io::{self, Stdout},
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Deserialize)]
struct NoteEvent {
    step: i32,
    duration_ms: u64,
}

#[derive(Debug, Clone, Copy)]
enum Waveform {
    Sine,
    Square,
}

impl Waveform {
    fn toggle(self) -> Self {
        match self {
            Waveform::Sine => Waveform::Square,
            Waveform::Square => Waveform::Sine,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Waveform::Sine => "sine",
            Waveform::Square => "square",
        }
    }
}

#[derive(Debug, Clone)]
struct PlayingNote {
    frequency: f32,
    waveform: Waveform,
    duration: Duration,
    started_at: Instant,
}

fn frequency_from_step(step: i32) -> f32 {
    440.0 * 2.0_f32.powf(step as f32 / 31.0)
}

struct ProceduralWave {
    waveform: Waveform,
    frequency: f32,
    sample_rate: u32,
    sample_clock: f32,
}

impl ProceduralWave {
    fn new(waveform: Waveform, frequency: f32, sample_rate: u32) -> Self {
        Self {
            waveform,
            frequency,
            sample_rate,
            sample_clock: 0.0,
        }
    }

    fn next_value(&mut self) -> f32 {
        let phase = 2.0 * PI * self.sample_clock;
        let value = match self.waveform {
            Waveform::Sine => phase.sin(),
            Waveform::Square => {
                if phase.sin() >= 0.0 {
                    1.0
                } else {
                    -1.0
                }
            }
        };

        self.sample_clock += self.frequency / self.sample_rate as f32;
        if self.sample_clock >= 1.0 {
            self.sample_clock -= 1.0;
        }

        value
    }
}

impl Iterator for ProceduralWave {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next_value())
    }
}

impl Source for ProceduralWave {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        1
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

fn load_sequence(path: &str) -> Result<Vec<NoteEvent>, String> {
    let data = fs::read_to_string(path)
        .map_err(|e| format!("Impossible de lire {}: {}", path, e))?;
    serde_json::from_str::<Vec<NoteEvent>>(&data)
        .map_err(|e| format!("Erreur JSON: {}", e))
}

fn spawn_audio_thread(
    sequence: Vec<NoteEvent>,
    sender: Sender<PlayingNote>,
    waveform_mode: Waveform,
) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || {
        let (_stream, handle) = OutputStream::try_default()
            .map_err(|e| format!("Erreur audio: {}", e))?;
        play_sequence(sequence, &handle, sender, waveform_mode)
    })
}

fn play_sequence(
    sequence: Vec<NoteEvent>,
    handle: &OutputStreamHandle,
    sender: Sender<PlayingNote>,
    mut waveform_mode: Waveform,
) -> Result<(), String> {
    let sink = Sink::try_new(handle).map_err(|e| format!("Erreur Sink: {}", e))?;

    for event in sequence {
        if event.step < 0 {
            return Err(format!("Step invalide: {} (doit être >= 0)", event.step));
        }

        let normalized_step = event.step.rem_euclid(31);
        let frequency = frequency_from_step(normalized_step);
        let duration = Duration::from_millis(event.duration_ms);

        sender
            .send(PlayingNote {
                frequency,
                waveform: waveform_mode,
                duration,
                started_at: Instant::now(),
            })
            .map_err(|e| format!("Erreur channel: {}", e))?;

        let source = ProceduralWave::new(waveform_mode, frequency, 44_100)
            .take_duration(duration)
            .amplify(0.2);

        sink.append(source);
        sink.sleep_until_end();

        waveform_mode = waveform_mode.toggle();
    }

    Ok(())
}

fn build_wave_points(note: &PlayingNote, sample_count: usize) -> Vec<(f64, f64)> {
    let mut points = Vec::with_capacity(sample_count);
    let speed = note.frequency / 220.0;

    for i in 0..sample_count {
        let x = i as f32 / (sample_count - 1) as f32;
        let phase = 2.0 * PI * (x * 2.0 * speed);
        let y = match note.waveform {
            Waveform::Sine => phase.sin(),
            Waveform::Square => {
                if phase.sin() >= 0.0 {
                    1.0
                } else {
                    -1.0
                }
            }
        };

        points.push((x as f64, y as f64));
    }

    points
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, io::Error> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<(), io::Error> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn draw_ui(terminal: &mut Terminal<CrosstermBackend<Stdout>>, note: Option<&PlayingNote>) -> io::Result<()> {
    terminal
        .draw(|frame| {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(10)])
            .split(frame.size());

        let info_text = if let Some(note) = note {
            let elapsed = note.started_at.elapsed().as_secs_f32();
            let progress = (elapsed / note.duration.as_secs_f32()).min(1.0);
            format!(
                "Note: {:.2} Hz | Waveform: {} | Progress: {:>3}%",
                note.frequency,
                note.waveform.label(),
                (progress * 100.0).round() as i32
            )
        } else {
            "Waiting for music...".to_string()
        };

        let info = Paragraph::new(info_text)
            .block(Block::default().title("31-EDO Player").borders(Borders::ALL));
        frame.render_widget(info, layout[0]);

        if let Some(note) = note {
            let points = build_wave_points(note, 200);
            let dataset = Dataset::default()
                .name(note.waveform.label())
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Cyan))
                .data(&points);

            let chart = Chart::new(vec![dataset])
                .block(Block::default().title("Oscilloscope").borders(Borders::ALL))
                .x_axis(ratatui::widgets::Axis::default().bounds([0.0, 1.0]))
                .y_axis(ratatui::widgets::Axis::default().bounds([-1.2, 1.2]));

            frame.render_widget(chart, layout[1]);
        } else {
            let chart = Chart::new(vec![])
                .block(Block::default().title("Oscilloscope").borders(Borders::ALL));
            frame.render_widget(chart, layout[1]);
        }
        })
        .map(|_| ())
}

fn main() -> Result<(), String> {
    let json_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "sequence.json".to_string());

    let sequence = load_sequence(&json_path)?;

    let (sender, receiver) = mpsc::channel();
    let audio_handle = spawn_audio_thread(sequence, sender, Waveform::Sine);

    let mut terminal = setup_terminal().map_err(|e| e.to_string())?;
    let mut current_note: Option<PlayingNote> = None;

    loop {
        while let Ok(note) = receiver.try_recv() {
            current_note = Some(note);
        }

        draw_ui(&mut terminal, current_note.as_ref()).map_err(|e| e.to_string())?;

        if event::poll(Duration::from_millis(16)).map_err(|e| e.to_string())? {
            if let Event::Key(key) = event::read().map_err(|e| e.to_string())? {
                if key.code == KeyCode::Char('q') {
                    break;
                }
            }
        }

        if audio_handle.is_finished() && receiver.try_recv().is_err() {
            thread::sleep(Duration::from_millis(300));
            break;
        }
    }

    restore_terminal(terminal).map_err(|e| e.to_string())?;
    Ok(())
}
