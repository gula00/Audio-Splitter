#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
mod app {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use anyhow::{anyhow, Context, Result};
    use eframe::egui;
    use eframe::egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
    use wasapi::{
        initialize_mta, Direction, Device, DeviceEnumerator, SampleType, StreamMode, WaveFormat,
    };

    const SAMPLE_RATE: usize = 48_000;
    const CHANNELS: usize = 2;
    const CHUNK_FRAMES: usize = 480;
    const HISTORY_LEN: usize = 180;

    pub fn run() -> Result<()> {
        let options = eframe::NativeOptions::default();
        eframe::run_native(
            "Audio Splitter",
            options,
            Box::new(|_cc| Ok(Box::new(SplitterApp::new()))),
        )
        .map_err(|e| anyhow!(e.to_string()))
    }

    struct SplitterApp {
        bridge: AudioBridge,
        source_name: String,
        devices: Vec<String>,
        selected: usize,
        status: String,
        input_history: VecDeque<f32>,
        output_history: VecDeque<f32>,
    }

    impl SplitterApp {
        fn new() -> Self {
            let devices = list_render_device_names().unwrap_or_default();
            let selected = devices
                .iter()
                .position(|d| d.to_ascii_lowercase().contains("cable input"))
                .unwrap_or(0);
            let source_name = default_render_name().unwrap_or_else(|_| "Default Speaker".to_string());

            Self {
                bridge: AudioBridge::new(),
                source_name,
                devices,
                selected,
                status: "idle".to_string(),
                input_history: VecDeque::from(vec![0.0; HISTORY_LEN]),
                output_history: VecDeque::from(vec![0.0; HISTORY_LEN]),
            }
        }

        fn selected_name(&self) -> String {
            self.devices
                .get(self.selected)
                .cloned()
                .unwrap_or_default()
        }

        fn refresh_devices(&mut self) {
            match list_render_device_names() {
                Ok(list) => {
                    self.devices = list;
                    if self.selected >= self.devices.len() {
                        self.selected = 0;
                    }
                    self.source_name = default_render_name().unwrap_or_else(|_| "Default Speaker".to_string());
                    self.status = "device list refreshed".to_string();
                }
                Err(err) => self.status = format!("refresh failed: {err}"),
            }
        }

        fn toggle(&mut self) {
            if self.bridge.is_running() {
                self.bridge.stop();
                self.status = "stopped".to_string();
                return;
            }

            let target = self.selected_name();
            if target.is_empty() {
                self.status = "no render device found; enable a virtual cable first".to_string();
                return;
            }

            match self.bridge.start(target.clone()) {
                Ok(()) => {
                    self.status = format!("running: speaker loopback -> {target}");
                }
                Err(err) => {
                    self.status = format!("start failed: {err}");
                }
            }
        }

        fn update_histories(&mut self) -> (f32, f32) {
            let (input, output) = self.bridge.levels();
            push_level(&mut self.input_history, input);
            push_level(&mut self.output_history, output);
            (input, output)
        }
    }

    impl eframe::App for SplitterApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            ctx.request_repaint_after(Duration::from_millis(16));
            let (in_level, out_level) = self.update_histories();

            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("Audio Splitter");
                ui.label("Input and output are shown as two columns. Running state draws a link line.");

                ui.horizontal(|ui| {
                    if ui.button("Refresh").clicked() {
                        self.refresh_devices();
                    }

                    let selected_text = self
                        .devices
                        .get(self.selected)
                        .map(|s| s.as_str())
                        .unwrap_or("<no device>");

                    egui::ComboBox::from_label("Output device")
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            for (i, name) in self.devices.iter().enumerate() {
                                ui.selectable_value(&mut self.selected, i, name);
                            }
                        });

                    let label = if self.bridge.is_running() { "Stop" } else { "Start" };
                    if ui
                        .add_sized([120.0, 30.0], egui::Button::new(label))
                        .clicked()
                    {
                        self.toggle();
                    }
                });

                ui.add_space(8.0);
                draw_flow_panel(
                    ui,
                    self.bridge.is_running(),
                    &self.source_name,
                    &self.selected_name(),
                    in_level,
                    out_level,
                    &self.input_history,
                    &self.output_history,
                    ctx.input(|i| i.time) as f32,
                );

                ui.add_space(8.0);
                ui.label(format!("Status: {}", self.status));
                ui.label("Virtual cable route example: app output -> CABLE Input, chat mic -> CABLE Output.");
            });
        }

        fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
            self.bridge.stop();
        }
    }

    struct AudioBridge {
        worker: Option<Worker>,
        meters: LevelMeters,
    }

    struct Worker {
        stop: Arc<AtomicBool>,
        handles: Vec<thread::JoinHandle<()>>,
    }

    #[derive(Clone)]
    struct LevelMeters {
        input: Arc<AtomicU32>,
        output: Arc<AtomicU32>,
    }

    impl LevelMeters {
        fn new() -> Self {
            Self {
                input: Arc::new(AtomicU32::new(0.0f32.to_bits())),
                output: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            }
        }

        fn set_input(&self, value: f32) {
            store_level(&self.input, value);
        }

        fn set_output(&self, value: f32) {
            store_level(&self.output, value);
        }

        fn get(&self) -> (f32, f32) {
            (load_level(&self.input), load_level(&self.output))
        }

        fn reset(&self) {
            self.set_input(0.0);
            self.set_output(0.0);
        }
    }

    impl AudioBridge {
        fn new() -> Self {
            Self {
                worker: None,
                meters: LevelMeters::new(),
            }
        }

        fn is_running(&self) -> bool {
            self.worker.is_some()
        }

        fn levels(&self) -> (f32, f32) {
            self.meters.get()
        }

        fn start(&mut self, target_render_name: String) -> Result<()> {
            if self.worker.is_some() {
                return Ok(());
            }

            let stop = Arc::new(AtomicBool::new(false));
            let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(12);
            let meters_capture = self.meters.clone();
            let meters_play = self.meters.clone();

            let stop_capture = stop.clone();
            let capture_handle = thread::Builder::new()
                .name("loopback-capture".to_string())
                .spawn(move || {
                    if let Err(err) = capture_loop(stop_capture, tx, meters_capture) {
                        eprintln!("capture loop ended: {err}");
                    }
                })
                .context("failed to start capture thread")?;

            let stop_play = stop.clone();
            let play_handle = thread::Builder::new()
                .name("virtual-mic-render".to_string())
                .spawn(move || {
                    if let Err(err) = playback_loop(stop_play, rx, &target_render_name, meters_play) {
                        eprintln!("playback loop ended: {err}");
                    }
                })
                .context("failed to start playback thread")?;

            self.worker = Some(Worker {
                stop,
                handles: vec![capture_handle, play_handle],
            });

            Ok(())
        }

        fn stop(&mut self) {
            if let Some(worker) = self.worker.take() {
                worker.stop.store(true, Ordering::Relaxed);
                for handle in worker.handles {
                    let _ = handle.join();
                }
            }
            self.meters.reset();
        }
    }

    fn capture_loop(stop: Arc<AtomicBool>, tx: SyncSender<Vec<u8>>, meters: LevelMeters) -> Result<()> {
        let _ = initialize_mta().ok();

        let enumerator = DeviceEnumerator::new()?;
        let loopback_source = enumerator
            .get_default_device(&Direction::Render)
            .context("failed to get default speaker device")?;

        let mut audio_client = loopback_source.get_iaudioclient()?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE, CHANNELS, None);
        let blockalign = format.get_blockalign() as usize;
        let chunk_bytes = CHUNK_FRAMES * blockalign;

        let (_, min_time) = audio_client.get_device_period()?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: min_time,
        };

        // Render endpoint + Capture direction => WASAPI loopback capture.
        audio_client.initialize_client(&format, &Direction::Capture, &mode)?;

        let event = audio_client.set_get_eventhandle()?;
        let capture_client = audio_client.get_audiocaptureclient()?;
        let mut queue = VecDeque::<u8>::with_capacity(chunk_bytes * 4);

        audio_client.start_stream()?;
        while !stop.load(Ordering::Relaxed) {
            capture_client.read_from_device_to_deque(&mut queue)?;

            while queue.len() >= chunk_bytes {
                let mut chunk = vec![0u8; chunk_bytes];
                for b in &mut chunk {
                    *b = queue.pop_front().unwrap_or(0);
                }

                meters.set_input(peak_level_from_f32le(&chunk));
                if tx.try_send(chunk).is_err() {
                    break;
                }
            }

            let _ = event.wait_for_event(200_000);
        }

        let _ = audio_client.stop_stream();
        meters.set_input(0.0);
        Ok(())
    }

    fn playback_loop(
        stop: Arc<AtomicBool>,
        rx: Receiver<Vec<u8>>,
        target_name: &str,
        meters: LevelMeters,
    ) -> Result<()> {
        let _ = initialize_mta().ok();

        let enumerator = DeviceEnumerator::new()?;
        let target_device = find_render_device_by_name(&enumerator, target_name)
            .with_context(|| format!("target device not found: {target_name}"))?;

        let mut audio_client = target_device.get_iaudioclient()?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE, CHANNELS, None);
        let blockalign = format.get_blockalign() as usize;

        let (_, min_time) = audio_client.get_device_period()?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: min_time,
        };
        audio_client.initialize_client(&format, &Direction::Render, &mode)?;

        let event = audio_client.set_get_eventhandle()?;
        let render_client = audio_client.get_audiorenderclient()?;
        let mut queue = VecDeque::<u8>::with_capacity(blockalign * 4096);

        audio_client.start_stream()?;

        while !stop.load(Ordering::Relaxed) {
            let frames = audio_client.get_available_space_in_frames()? as usize;
            if frames == 0 {
                let _ = event.wait_for_event(200_000);
                continue;
            }

            let need = frames * blockalign;
            while queue.len() < need {
                match rx.try_recv() {
                    Ok(chunk) => {
                        queue.extend(chunk);
                    }
                    Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => {
                        queue.resize(need, 0);
                        break;
                    }
                }
            }

            meters.set_output(peak_level_from_queue_prefix_f32le(&queue, need));
            render_client.write_to_device_from_deque(frames, &mut queue, None)?;
            let _ = event.wait_for_event(200_000);
        }

        let _ = audio_client.stop_stream();
        meters.set_output(0.0);
        Ok(())
    }

    fn draw_flow_panel(
        ui: &mut egui::Ui,
        running: bool,
        input_name: &str,
        output_name: &str,
        input_level: f32,
        output_level: f32,
        input_history: &VecDeque<f32>,
        output_history: &VecDeque<f32>,
        time: f32,
    ) {
        let width = ui.available_width();
        let height = 250.0;
        let (response, painter) = ui.allocate_painter(Vec2::new(width, height), Sense::hover());
        let rect = response.rect;

        let outer = rect.shrink2(Vec2::new(8.0, 8.0));
        let gap = 78.0;
        let card_w = ((outer.width() - gap).max(240.0)) * 0.5;
        let left = Rect::from_min_size(outer.left_top(), Vec2::new(card_w, outer.height()));
        let right = Rect::from_min_size(
            Pos2::new(outer.right() - card_w, outer.top()),
            Vec2::new(card_w, outer.height()),
        );

        draw_device_card(
            &painter,
            left,
            "Input",
            input_name,
            input_level,
            input_history,
            Color32::from_rgb(70, 180, 255),
        );
        draw_device_card(
            &painter,
            right,
            "Output",
            output_name,
            output_level,
            output_history,
            Color32::from_rgb(90, 235, 145),
        );

        let start = Pos2::new(left.right() + 12.0, left.center().y);
        let end = Pos2::new(right.left() - 12.0, right.center().y);
        let wire_color = if running {
            Color32::from_rgb(110, 220, 155)
        } else {
            Color32::from_gray(95)
        };
        painter.line_segment([start, end], Stroke::new(2.5, wire_color));

        if running {
            for i in 0..3 {
                let phase = ((time * 0.9) + (i as f32 * 0.33)).fract();
                let x = egui::lerp(start.x..=end.x, phase);
                let pulse = Rect::from_center_size(Pos2::new(x, start.y), Vec2::new(8.0, 8.0));
                painter.rect_filled(pulse, CornerRadius::same(4), Color32::from_rgb(120, 255, 180));
            }
        }
    }

    fn draw_device_card(
        painter: &egui::Painter,
        rect: Rect,
        title: &str,
        name: &str,
        level: f32,
        history: &VecDeque<f32>,
        accent: Color32,
    ) {
        let bg = Color32::from_rgb(22, 24, 28);
        painter.rect_filled(rect, CornerRadius::same(10), bg);
        painter.rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0, Color32::from_gray(68)),
            StrokeKind::Outside,
        );

        let title_pos = Pos2::new(rect.left() + 12.0, rect.top() + 10.0);
        painter.text(
            title_pos,
            Align2::LEFT_TOP,
            title,
            FontId::proportional(16.0),
            Color32::WHITE,
        );

        let name_pos = Pos2::new(rect.left() + 12.0, rect.top() + 34.0);
        painter.text(
            name_pos,
            Align2::LEFT_TOP,
            name,
            FontId::proportional(13.0),
            Color32::from_gray(190),
        );

        let waveform = Rect::from_min_max(
            Pos2::new(rect.left() + 12.0, rect.top() + 70.0),
            Pos2::new(rect.right() - 32.0, rect.bottom() - 14.0),
        );
        painter.rect_filled(waveform, CornerRadius::same(6), Color32::from_rgb(16, 17, 20));
        painter.rect_stroke(
            waveform,
            CornerRadius::same(6),
            Stroke::new(1.0, Color32::from_gray(50)),
            StrokeKind::Outside,
        );

        draw_waveform(painter, waveform.shrink2(Vec2::new(4.0, 6.0)), history, accent);

        let meter_bg = Rect::from_min_max(
            Pos2::new(rect.right() - 22.0, rect.top() + 70.0),
            Pos2::new(rect.right() - 10.0, rect.bottom() - 14.0),
        );
        painter.rect_filled(meter_bg, CornerRadius::same(5), Color32::from_gray(35));

        let meter_h = meter_bg.height() * level.clamp(0.0, 1.0);
        let meter_fill = Rect::from_min_max(
            Pos2::new(meter_bg.left(), meter_bg.bottom() - meter_h),
            meter_bg.right_bottom(),
        );
        painter.rect_filled(meter_fill, CornerRadius::same(5), accent);

        let db_text = format!("{:.0}%", level * 100.0);
        painter.text(
            Pos2::new(rect.right() - 8.0, rect.top() + 34.0),
            Align2::RIGHT_TOP,
            db_text,
            FontId::proportional(13.0),
            accent,
        );
    }

    fn draw_waveform(painter: &egui::Painter, rect: Rect, history: &VecDeque<f32>, color: Color32) {
        if history.len() < 2 {
            return;
        }

        let mid = rect.center().y;
        painter.line_segment(
            [Pos2::new(rect.left(), mid), Pos2::new(rect.right(), mid)],
            Stroke::new(1.0, Color32::from_gray(45)),
        );

        let mut points = Vec::with_capacity(history.len());
        let amp = rect.height() * 0.46;
        for (i, sample) in history.iter().enumerate() {
            let t = i as f32 / (history.len().saturating_sub(1) as f32);
            let x = egui::lerp(rect.left()..=rect.right(), t);
            let y = mid - sample.clamp(0.0, 1.0) * amp;
            points.push(Pos2::new(x, y));
        }
        painter.add(egui::Shape::line(points, Stroke::new(1.8, color)));
    }

    fn push_level(history: &mut VecDeque<f32>, level: f32) {
        if history.len() >= HISTORY_LEN {
            history.pop_front();
        }
        history.push_back(level.clamp(0.0, 1.0));
    }

    fn peak_level_from_f32le(bytes: &[u8]) -> f32 {
        if bytes.len() < 4 {
            return 0.0;
        }

        let mut peak = 0.0f32;
        for sample in bytes.chunks_exact(4) {
            let value = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
            if value.is_finite() {
                peak = peak.max(value.abs());
            }
        }
        peak.clamp(0.0, 1.0)
    }

    fn peak_level_from_queue_prefix_f32le(queue: &VecDeque<u8>, prefix_bytes: usize) -> f32 {
        let sample_bytes = prefix_bytes - (prefix_bytes % 4);
        if sample_bytes == 0 {
            return 0.0;
        }

        let mut peak = 0.0f32;
        let mut idx = 0usize;
        while idx < sample_bytes {
            let b0 = queue.get(idx).copied().unwrap_or(0);
            let b1 = queue.get(idx + 1).copied().unwrap_or(0);
            let b2 = queue.get(idx + 2).copied().unwrap_or(0);
            let b3 = queue.get(idx + 3).copied().unwrap_or(0);
            let value = f32::from_le_bytes([b0, b1, b2, b3]);
            if value.is_finite() {
                peak = peak.max(value.abs());
            }
            idx += 4;
        }
        peak.clamp(0.0, 1.0)
    }

    fn store_level(atom: &AtomicU32, value: f32) {
        atom.store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    fn load_level(atom: &AtomicU32) -> f32 {
        f32::from_bits(atom.load(Ordering::Relaxed)).clamp(0.0, 1.0)
    }

    fn default_render_name() -> Result<String> {
        let enumerator = DeviceEnumerator::new()?;
        let device = enumerator.get_default_device(&Direction::Render)?;
        Ok(device.get_friendlyname()?)
    }

    fn list_render_device_names() -> Result<Vec<String>> {
        let enumerator = DeviceEnumerator::new()?;
        let collection = enumerator.get_device_collection(&Direction::Render)?;
        let mut result = Vec::new();
        for dev in &collection {
            result.push(dev?.get_friendlyname()?);
        }
        result.sort();
        Ok(result)
    }

    fn find_render_device_by_name(enumerator: &DeviceEnumerator, needle: &str) -> Result<Device> {
        let collection = enumerator.get_device_collection(&Direction::Render)?;
        let needle_lc = needle.to_ascii_lowercase();

        for dev in &collection {
            let dev = dev?;
            let name = dev.get_friendlyname()?;
            if name.eq_ignore_ascii_case(needle) || name.to_ascii_lowercase().contains(&needle_lc) {
                return Ok(dev);
            }
        }

        Err(anyhow!("no matching render device"))
    }
}

#[cfg(target_os = "windows")]
fn main() {
    if let Err(err) = app::run() {
        eprintln!("{err}");
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("This project only supports Windows WASAPI.");
}
