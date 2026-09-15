use core::fmt::Display;
use std::{
	io::{Error as IoError, ErrorKind, Write},
	num::NonZeroU32
};

use crossterm::{
	cursor::MoveTo,
	event::EventStream,
	execute,
	terminal::{disable_raw_mode, enable_raw_mode}
};
use image::DynamicImage;
use kittage::{
	AsyncInputReader, ImageDimensions, ImageId, NumberOrId, PixelFormat, Verbosity,
	action::Action,
	delete::{ClearOrDelete, DeleteConfig, WhichToDelete},
	display::{CursorMovementPolicy, DisplayConfig, DisplayLocation},
	error::TransmitError,
	image::Image,
	medium::Medium
};
use ratatui::layout::Position;
use smallvec::SmallVec;

use crate::converter::MaybeTransferred;

pub struct KittyReadyToDisplay<'tui> {
	pub img: &'tui mut MaybeTransferred,
	pub page_num: usize,
	pub pos: Position,
	pub display_loc: DisplayLocation
}

pub enum KittyDisplay<'tui> {
	NoChange,
	ClearImages,
	DisplayImages(Vec<KittyReadyToDisplay<'tui>>)
}

pub struct DbgWriter<W: Write> {
	w: W,
	#[cfg(debug_assertions)]
	buf: String
}

struct TmuxChunkWriter<W: Write> {
	inner: W,
	buf: Vec<u8>
}

impl<W: Write> TmuxChunkWriter<W> {
	fn new(inner: W) -> Self {
		Self {
			inner,
			buf: Vec::new()
		}
	}

	fn write_wrapped(&mut self, sequence: &[u8]) -> std::io::Result<()> {
		self.inner.write_all(b"\x1bPtmux;")?;

		let mut last_written = 0;
		for (idx, byte) in sequence.iter().enumerate() {
			if *byte == b'\x1b' {
				self.inner.write_all(&sequence[last_written..=idx])?;
				self.inner.write_all(b"\x1b")?;
				last_written = idx + 1;
			}
		}
		self.inner.write_all(&sequence[last_written..])?;
		self.inner.write_all(b"\x1b\\")
	}
}

impl<W: Write> Write for TmuxChunkWriter<W> {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		self.buf.extend_from_slice(buf);
		Ok(buf.len())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		let buf = std::mem::take(&mut self.buf);
		let mut sequence_start = 0;

		while sequence_start < buf.len() {
			let Some(terminator) = buf[sequence_start..]
				.windows(2)
				.position(|window| window == b"\x1b\\")
			else {
				return Err(IoError::new(
					ErrorKind::InvalidData,
					"Kitty command did not end with ST"
				));
			};
			let sequence_end = sequence_start + terminator + 2;
			self.write_wrapped(&buf[sequence_start..sequence_end])?;
			sequence_start = sequence_end;
		}

		self.inner.flush()
	}
}

impl<W: Write> Write for DbgWriter<W> {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		#[cfg(debug_assertions)]
		{
			if let Ok(s) = std::str::from_utf8(buf) {
				self.buf.push_str(s);
			}
		}
		self.w.write(buf)
	}

	fn flush(&mut self) -> std::io::Result<()> {
		#[cfg(debug_assertions)]
		{
			log::debug!("Writing to kitty: {:?}", self.buf);
			self.buf.clear();
		}
		self.w.flush()
	}
}

pub async fn run_action<'es>(
	action: Action<'_, '_>,
	ev_stream: &'es mut EventStream,
	is_tmux: bool
) -> Result<Option<ImageId>, TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>> {
	if is_tmux {
		let image_id = match &action {
			Action::Transmit(image) => match image.num_or_id {
				NumberOrId::Id(id) => Some(id),
				NumberOrId::Number(_) => None
			},
			Action::Query(image) => match image.num_or_id {
				NumberOrId::Id(id) => Some(id),
				NumberOrId::Number(_) => None
			},
			Action::TransmitAndDisplay { image, .. } => match image.num_or_id {
				NumberOrId::Id(id) => Some(id),
				NumberOrId::Number(_) => None
			},
			Action::Display { image_id, .. } => Some(*image_id),
			Action::Delete(_) => None
		};

		action
			.write_transmit_to(
				TmuxChunkWriter::new(std::io::stdout().lock()),
				Verbosity::Silent
			)
			.map_err(TransmitError::Writing)?;
		return Ok(image_id);
	}

	let writer = DbgWriter {
		w: std::io::stdout().lock(),
		#[cfg(debug_assertions)]
		buf: String::new()
	};
	action
		.execute_async(writer, ev_stream)
		.await
		.map(|(_, i)| i)
}

pub async fn do_shms_work(ev_stream: &mut EventStream) -> bool {
	let img = DynamicImage::new_rgb8(1, 1);
	let pid = std::process::id();
	let shm_name = format!("tdf_test_{pid}");

	#[cfg(unix)]
	let shm_name = &*shm_name;

	let Ok(mut k_img) = kittage::image::Image::shm_from(img, shm_name) else {
		return false;
	};

	// apparently the terminal won't respond to queries unless they have an Id instead of a number
	k_img.num_or_id = NumberOrId::Id(NonZeroU32::new(u32::MAX).unwrap());

	enable_raw_mode().unwrap();

	let res = run_action(Action::Query(&k_img), ev_stream, false).await;

	disable_raw_mode().unwrap();

	res.is_ok()
}

type ESTransErr<'es> = TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>;

pub struct DisplayErr<'es> {
	pub failed_pages: SmallVec<[usize; 2]>,
	pub user_facing_err: &'static str,
	pub source: DisplayErrSource<'es>
}

impl<'es> DisplayErr<'es> {
	fn empty(user_facing_err: &'static str, source: ESTransErr<'es>) -> Self {
		Self {
			failed_pages: SmallVec::new(),
			user_facing_err,
			source: DisplayErrSource::Transmission(source)
		}
	}
}

#[derive(Debug)]
pub enum DisplayErrSource<'es> {
	KittageReturnedNoId,
	Transmission(ESTransErr<'es>)
}

impl Display for DisplayErrSource<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::KittageReturnedNoId => write!(
				f,
				"Kittage returned no ID when we asked it to display an image. This is a bug in kittage, please report it."
			),
			Self::Transmission(t) => write!(f, "Error with talking to the terminal: {t}")
		}
	}
}

pub async fn display_kitty_images<'es>(
	display: KittyDisplay<'_>,
	ev_stream: &'es mut EventStream,
	last_z_index: &mut i32,
	is_tmux: bool
) -> Result<(), DisplayErr<'es>> {
	let images = match display {
		KittyDisplay::NoChange => return Ok(()),
		KittyDisplay::ClearImages =>
			return run_action(
				Action::Delete(DeleteConfig {
					effect: ClearOrDelete::Clear,
					which: WhichToDelete::All
				}),
				ev_stream,
				is_tmux
			)
			.await
			.map_err(|e| DisplayErr::empty("Couldn't clear previous images", e))
			.map(|_: Option<ImageId>| ()),
		KittyDisplay::DisplayImages(imgs) => imgs
	};

	let new_z_index = last_z_index.wrapping_add_unsigned(1);

	let mut err = Ok::<(), (SmallVec<[usize; 2]>, DisplayErrSource<'es>)>(());
	for KittyReadyToDisplay {
		img,
		page_num,
		pos,
		mut display_loc
	} in images
	{
		display_loc.z_index = new_z_index;

		let config = DisplayConfig {
			location: display_loc,
			cursor_movement: CursorMovementPolicy::DontMove,
			..DisplayConfig::default()
		};

		execute!(std::io::stdout(), MoveTo(pos.x, pos.y)).unwrap();

		log::debug!("going to display img {img:#?}");
		log::debug!("displaying with config {config:#?}");

		let this_err = match img {
			MaybeTransferred::NotYet(image) => {
				let placement_id = match (is_tmux, image.num_or_id) {
					(true, NumberOrId::Id(id)) => Some(id),
					_ => None
				};
				let mut fake_image = Image {
					num_or_id: image.num_or_id,
					format: PixelFormat::Rgb24(
						ImageDimensions {
							width: 0,
							height: 0
						},
						None
					),
					medium: Medium::Direct {
						chunk_size: None,
						data: (&[]).into()
					}
				};
				std::mem::swap(image, &mut fake_image);

				run_action(
					Action::TransmitAndDisplay {
						image: fake_image,
						config,
						placement_id
					},
					ev_stream,
					is_tmux
				)
				.await
				.map_err(DisplayErrSource::Transmission)
				.and_then(|img_id| {
					img_id
						.map(|id| *img = MaybeTransferred::Transferred(id))
						.ok_or(DisplayErrSource::KittageReturnedNoId)
				})
			}
			MaybeTransferred::Transferred(image_id) => run_action(
				Action::Display {
					image_id: *image_id,
					placement_id: *image_id,
					config
				},
				ev_stream,
				is_tmux
			)
			.await
			// don't need the return id 'cause we already know it
			.map(|_: Option<ImageId>| ())
			.map_err(DisplayErrSource::Transmission)
		};

		log::debug!("this_err is {this_err:#?}");

		if let Err(e) = this_err {
			match err.as_mut() {
				Ok(()) => err = Err((SmallVec::from([page_num].as_slice()), e)),
				Err((v, _)) => v.push(page_num)
			}
		}
	}

	let z_idxes_to_remove = *last_z_index;
	*last_z_index = new_z_index;

	match err {
		Err((failed_pages, source)) => Err(DisplayErr {
			failed_pages,
			user_facing_err: "Couldn't transfer image to the terminal",
			source
		}),
		Ok(()) => run_action(
			Action::Delete(DeleteConfig {
				effect: ClearOrDelete::Clear,
				which: WhichToDelete::PlacementsWithZIndex(z_idxes_to_remove)
			}),
			ev_stream,
			is_tmux
		)
		.await
		.map_err(|e| DisplayErr::empty("Couldn't clear previously-sent images", e))
		.map(|_| ())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn wraps_each_kitty_chunk_separately_for_tmux() {
		let mut writer = TmuxChunkWriter::new(Vec::new());
		writer
			.write_all(b"\x1b_Gm=1;first\x1b\\\x1b_Gm=0;second\x1b\\")
			.unwrap();
		writer.flush().unwrap();

		assert_eq!(
			writer.inner,
			b"\x1bPtmux;\x1b\x1b_Gm=1;first\x1b\x1b\\\x1b\\\x1bPtmux;\x1b\x1b_Gm=0;second\x1b\x1b\\\x1b\\"
		);
	}
}
