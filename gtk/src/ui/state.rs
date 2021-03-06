use super::{hash, App, FlashTask, OpenDialog};
use super::super::BlockDevice;

use std::mem;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

use gtk;
use gtk::*;
use popsicle::{self, DiskError};

pub struct BufferingData {
    pub data:  Mutex<(PathBuf, Vec<u8>)>,
    pub state: AtomicUsize,
}

impl BufferingData {
    pub fn new() -> BufferingData {
        BufferingData {
            data:  Mutex::new((PathBuf::new(), Vec::new())),
            state: 0.into(),
        }
    }
}

pub struct Connected(App);

impl Connected {
    /// Display the window, and execute the gtk main event loop.
    pub fn then_execute(self) {
        self.0.window.show_all();
        gtk::main();
    }
}

pub trait Connect {
    /// Creates external state, and maps all of the UI functionality to the UI.
    fn connect_events(self) -> Connected;

    /// Programs the button for selecting an image.
    fn connect_image_chooser(&self);

    /// Programs the combo box which generates the hash sum for initial image selection view.
    fn connect_hash_generator(&self);

    /// Programs the back button, whose behavior changes based on the currently active view.
    fn connect_back_button(&self);

    /// Programs the next button, whose behavior changes based on the currently active view.
    fn connect_next_button(&self);

    /// Programs the action that will be performed when the check all button is clicked.
    fn connect_check_all(&self);

    /// Adds a function for GTK to execute when the application is idle, to monitor and
    /// update the progress bars for devices that are being flashed, and to generate
    /// the summary view after all devices have been flashed.
    fn watch_flashing_devices(&self);
}

impl Connect for App {
    fn connect_events(self) -> Connected {
        self.connect_image_chooser();
        self.connect_hash_generator();
        self.connect_back_button();
        self.connect_next_button();
        self.connect_check_all();
        self.watch_flashing_devices();

        Connected(self)
    }

    fn connect_image_chooser(&self) {
        let state = self.state.clone();
        self.content.image_view.chooser.connect_clicked(move |_| {
            if let Some(path) = OpenDialog::new(None).run() {
                let _ = state.image_sender.send(path);
            }
        });
    }

    fn connect_hash_generator(&self) {
        let state = self.state.clone();
        let hash_label = self.content.image_view.hash_label.clone();
        self.content.image_view.hash.connect_changed(move |hash| {
            if state.buffer.state.load(Ordering::SeqCst) == 0b010 {
                let (_, ref data) = *state.buffer.data.lock().unwrap();
                hash_label.set_icon_from_icon_name(EntryIconPosition::Primary, "gnome-spinner");
                hash_label.set_icon_sensitive(EntryIconPosition::Primary, true);
                hash::set(&hash_label, hash.get_active_text().unwrap().as_str(), data);
                hash_label.set_icon_sensitive(EntryIconPosition::Primary, false);
            }
        });
    }

    fn connect_back_button(&self) {
        let stack = self.content.container.clone();
        let back = self.header.back.clone();
        let next = self.header.next.clone();
        let state = self.state.clone();
        back.connect_clicked(move |back| {
            let view = state.view.get();
            match view {
                0 => gtk::main_quit(),
                1 => {
                    stack.set_transition_type(StackTransitionType::SlideRight);
                    stack.set_visible_child_name("image");
                    back.set_label("Cancel");
                    next.set_label("Next");
                    next.set_sensitive(true);
                    next.get_style_context().map(|c| {
                        c.remove_class("destructive-action");
                        c.add_class("suggested-action");
                    });
                }
                _ => unreachable!(),
            }

            state.view.set(view - 1);
        });
    }

    fn connect_next_button(&self) {
        let back = self.header.back.clone();
        let list = self.content.devices_view.list.clone();
        let next = self.header.next.clone();
        let stack = self.content.container.clone();
        let summary_grid = self.content.flash_view.progress_list.clone();
        let state = self.state.clone();

        next.connect_clicked(move |next| {
            let device_list = &state.devices;
            state.buffer.state.store(0b1000, Ordering::SeqCst);
            let (_, ref mut image_data) = *state.buffer.data.lock().unwrap();
            let start = &state.start;
            let task_handles = &state.task_handles;
            let bars = &state.bars;
            let tasks = &state.tasks;
            let view = &state.view;
            let view_value = view.get();
            stack.set_transition_type(StackTransitionType::SlideLeft);

            match view_value {
                // Move to device selection screen
                0 => {
                    back.set_label("Back");
                    next.set_label("Flash");
                    next.get_style_context().map(|c| {
                        c.remove_class("suggested-action");
                        c.add_class("destructive-action");
                    });
                    stack.set_visible_child_name("devices");

                    // Remove all but the first row
                    list.get_children()
                        .into_iter()
                        .skip(1)
                        .for_each(|widget| widget.destroy());

                    let mut devices = vec![];
                    if let Err(why) = popsicle::get_disk_args(&mut devices) {
                        eprintln!("popsicle: unable to get devices: {}", why);
                    }

                    let mut device_list = device_list.lock().unwrap();
                    device_list.clear();
                    for device in &devices {
                        let name = Path::new(&device).canonicalize().unwrap();
                        let button = if let Some(block) = BlockDevice::new(&name) {
                            CheckButton::new_with_label(&[
                                &block.label(),
                                " (",
                                &name.to_string_lossy(),
                                ")",
                            ].concat())
                        } else {
                            CheckButton::new_with_label(&name.to_string_lossy())
                        };

                        list.insert(&button, -1);
                        device_list.push((device.clone(), button));
                    }

                    list.show_all();
                }
                // Begin the device flashing process
                1 => {
                    let device_list = device_list.lock().unwrap();
                    let devs = device_list.iter().map(|x| x.0.clone());
                    // TODO: Handle Error
                    let mounts = popsicle::Mount::all().unwrap();
                    // TODO: Handle Error
                    let disks = popsicle::disks_from_args(devs, &mounts, true).unwrap();

                    back.set_visible(false);
                    next.set_visible(false);
                    stack.set_visible_child_name("flash");

                    // Clear the progress bar summaries.
                    let mut bars = bars.borrow_mut();
                    bars.clear();
                    summary_grid.get_children().iter().for_each(|c| c.destroy());

                    *start.borrow_mut() = Instant::now();
                    let mut tasks = tasks.lock().unwrap();
                    let mut task_handles = task_handles.lock().unwrap();

                    // Take ownership of the data, so that we may wrap it within an `Arc`
                    // and redistribute it across threads.
                    //
                    // Note: Possible optimization could be done to avoid the wrap.
                    //       Avoiding the wrap could eliminate two allocations.
                    let mut data = Vec::new();
                    mem::swap(&mut data, image_data);
                    let image_data = Arc::new(data);

                    for (id, (disk_path, mut disk)) in disks.into_iter().enumerate() {
                        let id = id as i32;
                        let image_data = image_data.clone();
                        let progress = Arc::new(AtomicUsize::new(0));
                        let finished = Arc::new(AtomicUsize::new(0));
                        let bar = ProgressBar::new();
                        bar.set_hexpand(true);

                        let label = {
                            let disk_path = Path::new(&disk_path).canonicalize().unwrap();
                            if let Some(block) = BlockDevice::new(&disk_path) {
                                Label::new(
                                    [&block.label(), " (", &disk_path.to_string_lossy(), ")"]
                                        .concat()
                                        .as_str(),
                                )
                            } else {
                                Label::new(disk_path.to_string_lossy().as_ref())
                            }
                        };

                        label.set_justify(Justification::Right);
                        label
                            .get_style_context()
                            .map(|c| c.add_class("progress-label"));
                        let bar_label = Label::new("");
                        bar_label.set_halign(Align::Center);
                        let bar_container = Box::new(Orientation::Vertical, 0);
                        bar_container.pack_start(&bar, false, false, 0);
                        bar_container.pack_start(&bar_label, false, false, 0);
                        summary_grid.attach(&label, 0, id, 1, 1);
                        summary_grid.attach(&bar_container, 1, id, 1, 1);
                        bars.push((bar, bar_label));

                        // Spawn a thread that will update the progress value over time.
                        //
                        // The value will be stored within an intermediary atomic integer,
                        // because it is unsafe to send GTK widgets across threads.
                        task_handles.push({
                            let progress = progress.clone();
                            let finished = finished.clone();
                            thread::spawn(move || -> Result<(), DiskError> {
                                let result = popsicle::write_to_disk(
                                    |_msg| (),
                                    || (),
                                    |value| progress.store(value as usize, Ordering::SeqCst),
                                    disk,
                                    disk_path,
                                    image_data.len() as u64,
                                    &image_data,
                                    false,
                                );

                                finished.store(1, Ordering::SeqCst);
                                result
                            })
                        });

                        tasks.push(FlashTask {
                            previous: Arc::new(Mutex::new([0; 7])),
                            progress,
                            finished,
                        });
                    }

                    summary_grid.show_all();
                }
                2 => gtk::main_quit(),
                _ => unreachable!(),
            }

            view.set(view_value + 1);
        });
    }

    fn connect_check_all(&self) {
        let all = self.content.devices_view.select_all.clone();
        let state = self.state.clone();
        all.connect_clicked(move |all| {
            if all.get_active() {
                state
                    .devices
                    .lock()
                    .unwrap()
                    .iter()
                    .for_each(|&(_, ref device)| device.set_active(true));
            }
        });
    }

    fn watch_flashing_devices(&self) {
        let stack = self.content.container.clone();
        let next = self.header.next.clone();
        let description = self.content.summary_view.description.clone();
        let list = self.content.summary_view.list.clone();
        let state = self.state.clone();
        let image_label = self.content.image_view.image_path.clone();
        let chooser_container = self.content.image_view.chooser_container.clone();

        gtk::timeout_add(500, move || {
            let tasks = &state.tasks;
            let bars = &state.bars;
            let devices = &state.devices;
            let task_handles = &state.task_handles;
            let image_length = &state.image_length;

            // Ensure that the image has been loaded before continuing.
            match state.buffer.state.load(Ordering::SeqCst) {
                0b0000 => {
                    return Continue(true);
                }
                0b0001 => {
                    chooser_container.set_visible_child_name("loader");
                    next.set_sensitive(false);
                    return Continue(true);
                }
                0b0010 => {
                    chooser_container.set_visible_child_name("chooser");
                    let (ref path, ref data) = *state.buffer.data.lock().unwrap();
                    next.set_sensitive(true);
                    image_label.set_text(&path.file_name().unwrap().to_string_lossy());
                    image_length.set(data.len());
                }
                0b0100 => {
                    chooser_container.set_visible_child_name("chooser");
                    next.set_sensitive(false);
                    return Continue(true);
                }
                0b1000 => (),
                _ => unreachable!(),
            }

            let image_length = image_length.get();

            let tasks = tasks.lock().unwrap();
            let ntasks = tasks.len();
            if ntasks == 0 {
                return Continue(true);
            }

            let mut finished = true;
            for (task, &(ref bar, ref label)) in tasks.deref().iter().zip(bars.borrow().iter()) {
                let raw_value = task.progress.load(Ordering::SeqCst);
                let value = if task.finished.load(Ordering::SeqCst) == 1 {
                    1.0f64
                } else {
                    finished = false;
                    raw_value as f64 / image_length as f64
                };

                bar.set_fraction(value);

                let mut prev_values = task.previous.lock().unwrap();
                prev_values[1] = prev_values[2];
                prev_values[2] = prev_values[3];
                prev_values[3] = prev_values[4];
                prev_values[4] = prev_values[5];
                prev_values[5] = prev_values[6];
                prev_values[6] = raw_value - prev_values[0];
                prev_values[0] = raw_value;

                let sum: usize = prev_values.iter().skip(1).sum();
                let per_second = sum / 3;
                label.set_label(&if per_second > (1024 * 1024) {
                    format!("{} MiB/s", per_second / (1024 * 1024))
                } else {
                    format!("{} KiB/s", per_second / 1024)
                });
            }

            if finished {
                stack.set_visible_child_name("summary");
                next.set_label("Close");
                next.get_style_context()
                    .map(|c| c.remove_class("destructive-action"));
                next.set_visible(true);

                let mut errored: Vec<(String, DiskError)> = Vec::new();
                let mut task_handles = task_handles.lock().unwrap();
                let devices = devices.lock().unwrap();
                let handle_iter = task_handles.deref_mut().drain(..);
                let mut device_iter = devices.deref().iter();
                for handle in handle_iter {
                    if let Some(&(ref device, _)) = device_iter.next() {
                        if let Err(why) = handle.join().unwrap() {
                            errored.push((device.clone(), why));
                        }
                    }
                }

                if errored.is_empty() {
                    description.set_text(&format!("{} devices successfully flashed", ntasks));
                } else {
                    description.set_text(&format!(
                        "{} of {} devices successfully flashed",
                        ntasks - errored.len(),
                        ntasks
                    ));
                    list.set_visible(true);
                    for (device, why) in errored {
                        let container = Box::new(Orientation::Horizontal, 0);
                        let device = Label::new(device.as_str());
                        let why = Label::new(format!("{}", why).as_str());
                        container.pack_start(&device, false, false, 0);
                        container.pack_start(&why, true, true, 0);
                        list.insert(&container, -1);
                    }
                }

                Continue(false)
            } else {
                Continue(true)
            }
        });
    }
}
