//! Spectrum plot, waterfall, and waterfall texture updates.

use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};

use crate::app::MidoriApp;
use crate::app::format::db_to_color;

impl MidoriApp {
    pub(crate) fn rebuild_waterfall_texture(&mut self, ctx: &egui::Context) {
        let width = self.config.ui.waterfall_width;
        let height = self.config.ui.waterfall_rows;
        let mut pixels = vec![egui::Color32::BLACK; width * height];

        for (row_index, row) in self.waterfall_rows.iter().rev().enumerate() {
            if row_index >= height {
                break;
            }
            for (column, &db) in row.iter().enumerate().take(width) {
                pixels[row_index * width + column] =
                    db_to_color(db, self.db_min, self.db_max, self.colormap);
            }
        }

        let image = egui::ColorImage {
            size: [width, height],
            pixels,
            source_size: egui::Vec2::new(width as f32, height as f32),
        };

        match &mut self.waterfall_texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.waterfall_texture =
                    Some(ctx.load_texture("waterfall", image, egui::TextureOptions::LINEAR));
            }
        }
    }
    pub(crate) fn draw_central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let total = ui.available_height().max(120.0);
            let max_split = (total - 70.0).max(60.0);
            let split = (total * self.spectrum_ratio).clamp(60.0, max_split);

            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), split),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    self.draw_spectrum(ui);
                },
            );

            let separator = ui
                .allocate_response(egui::vec2(ui.available_width(), 6.0), egui::Sense::drag())
                .on_hover_cursor(egui::CursorIcon::ResizeVertical);
            ui.painter()
                .rect_filled(separator.rect, 1.0, egui::Color32::from_rgb(45, 55, 68));
            if separator.dragged() {
                let delta = ui.input(|input| input.pointer.delta().y);
                self.spectrum_ratio = ((split + delta) / total).clamp(0.2, 0.85);
            }

            self.draw_waterfall(ui);
        });
    }

    pub(crate) fn draw_spectrum(&mut self, ui: &mut egui::Ui) {
        let mut tune_to = None;
        let reset = self.reset_spectrum_view;
        self.reset_spectrum_view = false;

        let plot = Plot::new("spectrum_plot")
            .x_axis_label("Frequency (MHz)")
            .y_axis_label("Power (dBFS)")
            .auto_bounds([true, false])
            .include_y(self.db_min as f64)
            .include_y(self.db_max as f64)
            .allow_drag([true, false])
            .allow_zoom([true, false])
            .show_axes([true, true])
            .label_formatter(|_name, value| format!("{:.4} MHz\n{:.1} dBFS", value.x, value.y));
        let plot = if reset { plot.reset() } else { plot };

        let plot_response = plot.show(ui, |plot_ui| {
            // The dB scale is a receiver setting, not a data-driven range.
            // Forcing it every frame stops the y axis from jumping with the
            // signal when replaying recordings.
            plot_ui.set_plot_bounds_y(self.db_min as f64..=self.db_max as f64);

            if !self.current_spectrum.is_empty() {
                let points = PlotPoints::new(self.current_spectrum.clone());
                let line = Line::new("Power", points)
                    .color(egui::Color32::from_rgb(30, 160, 255))
                    .width(1.2);
                plot_ui.line(line);
            }

            let center_mhz = self.center_freq_hz / 1e6;
            let vfo_mhz = (self.center_freq_hz + self.vfo_offset_hz) / 1e6;
            let half_bw_mhz = self.demod_bandwidth_hz() / 2.0 / 1e6;
            for (name, x) in [
                ("VFO low", vfo_mhz - half_bw_mhz),
                ("VFO high", vfo_mhz + half_bw_mhz),
            ] {
                let edges = Line::new(
                    name,
                    PlotPoints::new(vec![[x, self.db_min as f64], [x, self.db_max as f64]]),
                )
                .color(egui::Color32::from_rgb(90, 110, 130))
                .width(0.8);
                plot_ui.line(edges);
            }
            let vfo = Line::new(
                "VFO",
                PlotPoints::new(vec![
                    [vfo_mhz, self.db_min as f64],
                    [vfo_mhz, self.db_max as f64],
                ]),
            )
            .color(egui::Color32::from_rgb(80, 220, 255))
            .width(1.0);
            plot_ui.line(vfo);
            if (vfo_mhz - center_mhz).abs() > 1e-9 {
                let lo = Line::new(
                    "LO",
                    PlotPoints::new(vec![
                        [center_mhz, self.db_min as f64],
                        [center_mhz, self.db_max as f64],
                    ]),
                )
                .color(egui::Color32::from_rgb(70, 80, 95))
                .width(0.8);
                plot_ui.line(lo);
            }

            if plot_ui.response().clicked()
                && let Some(point) = plot_ui.pointer_coordinate()
            {
                tune_to = Some(point.x);
            }
        });

        let bounds = plot_response.transform.bounds();
        if bounds.is_valid_x() {
            self.spectrum_view_x = Some((bounds.min()[0], bounds.max()[0]));
        }

        if let Some(mhz) = tune_to {
            self.set_frequency_mhz(mhz);
        }
    }

    pub(crate) fn draw_waterfall(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let Some(texture_id) = self.waterfall_texture.as_ref().map(|texture| texture.id()) else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("Waiting for IQ\nConnect the Pluto first")
                        .size(18.0)
                        .color(egui::Color32::GRAY),
                );
            });
            return;
        };

        let response = ui
            .allocate_response(available, egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::Crosshair);
        let rect = response.rect;
        let full_min = (self.center_freq_hz - self.sample_rate_hz / 2.0) / 1e6;
        let full_max = (self.center_freq_hz + self.sample_rate_hz / 2.0) / 1e6;
        let full_span = (full_max - full_min).max(1e-12);

        let mut view_min = full_min;
        let mut view_max = full_max;
        if self.waterfall_follow
            && let Some((low, high)) = self.spectrum_view_x
        {
            let low = low.max(full_min);
            let high = high.min(full_max);
            if high - low > full_span * 1e-6 {
                view_min = low;
                view_max = high;
            }
        }
        let span = (view_max - view_min).max(1e-12);
        let uv = egui::Rect::from_min_max(
            egui::pos2(
                ((view_min - full_min) / full_span).clamp(0.0, 1.0) as f32,
                0.0,
            ),
            egui::pos2(
                ((view_max - full_min) / full_span).clamp(0.0, 1.0) as f32,
                1.0,
            ),
        );
        ui.painter()
            .image(texture_id, rect, uv, egui::Color32::WHITE);

        let painter = ui.painter();
        let x_of = |frequency_mhz: f64| {
            rect.min.x + ((frequency_mhz - view_min) / span) as f32 * rect.width()
        };
        let vfo_mhz = (self.center_freq_hz + self.vfo_offset_hz) / 1e6;
        let half_bw_mhz = self.demod_bandwidth_hz() / 2.0 / 1e6;
        let x_center = x_of(vfo_mhz);
        let x_low = x_of(vfo_mhz - half_bw_mhz).clamp(rect.min.x, rect.max.x);
        let x_high = x_of(vfo_mhz + half_bw_mhz).clamp(rect.min.x, rect.max.x);
        if x_low < rect.max.x && x_high > rect.min.x {
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x_low, rect.min.y),
                    egui::pos2(x_high, rect.max.y),
                ),
                0.0,
                egui::Color32::from_rgba_unmultiplied(30, 200, 255, 18),
            );
        }
        if (rect.min.x..=rect.max.x).contains(&x_center) {
            painter.line_segment(
                [
                    egui::pos2(x_center, rect.min.y),
                    egui::pos2(x_center, rect.max.y),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 220, 255)),
            );
        }

        // In independent mode the waterfall keeps the full captured band. Mark
        // the part that the spectrum plot is currently showing.
        if !self.waterfall_follow
            && let Some((low, high)) = self.spectrum_view_x
        {
            let low = low.max(full_min);
            let high = high.min(full_max);
            if high - low > full_span * 1e-6 {
                let x_low = rect.min.x + ((low - full_min) / full_span) as f32 * rect.width();
                let x_high = rect.min.x + ((high - full_min) / full_span) as f32 * rect.width();
                let shade = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 90);
                if x_low > rect.min.x {
                    painter.rect_filled(
                        egui::Rect::from_min_max(rect.min, egui::pos2(x_low, rect.max.y)),
                        0.0,
                        shade,
                    );
                }
                if x_high < rect.max.x {
                    painter.rect_filled(
                        egui::Rect::from_min_max(egui::pos2(x_high, rect.min.y), rect.max),
                        0.0,
                        shade,
                    );
                }
                let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(230, 200, 80));
                painter.line_segment(
                    [egui::pos2(x_low, rect.min.y), egui::pos2(x_low, rect.max.y)],
                    stroke,
                );
                painter.line_segment(
                    [
                        egui::pos2(x_high, rect.min.y),
                        egui::pos2(x_high, rect.max.y),
                    ],
                    stroke,
                );
                painter.text(
                    egui::pos2((x_low + x_high) * 0.5, rect.min.y + 4.0),
                    egui::Align2::CENTER_TOP,
                    format!("spectrum view {low:.3}–{high:.3} MHz"),
                    egui::FontId::proportional(10.0),
                    egui::Color32::from_rgb(230, 200, 80),
                );
            }
        }

        let steps = 5;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let db = self.db_min + t * (self.db_max - self.db_min);
            let y = rect.min.y + (1.0 - t) * rect.height();
            painter.text(
                egui::pos2(rect.max.x - 6.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{db:.0} dB"),
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
        }

        for i in 0..=4 {
            let t = i as f32 / 4.0;
            let frequency = view_min + t as f64 * span;
            let x = rect.min.x + t * rect.width();
            painter.text(
                egui::pos2(x, rect.max.y - 2.0),
                egui::Align2::CENTER_BOTTOM,
                format!("{frequency:.3}"),
                egui::FontId::proportional(10.0),
                egui::Color32::from_gray(200),
            );
        }

        if self.spectrum_view_x.is_some() {
            painter.text(
                egui::pos2(rect.min.x + 6.0, rect.min.y + 4.0),
                egui::Align2::LEFT_TOP,
                if self.waterfall_follow {
                    "waterfall: follows spectrum"
                } else {
                    "waterfall: full band"
                },
                egui::FontId::proportional(10.0),
                egui::Color32::from_gray(170),
            );
        }

        if response.clicked()
            && let Some(position) = response.interact_pointer_pos()
        {
            let fraction = ((position.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
            let frequency = view_min + fraction as f64 * span;
            self.set_frequency_mhz(frequency);
        }
    }
}
