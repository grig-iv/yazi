use std::{borrow::Cow, collections::VecDeque, fs::Metadata, path::{Path, PathBuf}};

use anyhow::Result;
use futures::{future::BoxFuture, FutureExt};
use tokio::{fs, io::{self, ErrorKind::{AlreadyExists, NotFound}}, sync::mpsc};
use tracing::warn;
use yazi_config::TASKS;
use yazi_shared::fs::{calculate_size, copy_with_progress, path_relative_to, Url};

use super::{FileOp, FileOpDelete, FileOpLink, FileOpPaste, FileOpTrash};
use crate::{TaskOp, TaskProg, LOW, NORMAL};

pub struct File {
	macro_: async_priority_channel::Sender<TaskOp, u8>,
	prog:   mpsc::UnboundedSender<TaskProg>,
}

impl File {
	pub fn new(
		macro_: async_priority_channel::Sender<TaskOp, u8>,
		prog: mpsc::UnboundedSender<TaskProg>,
	) -> Self {
		Self { macro_, prog }
	}

	pub async fn work(&self, op: FileOp) -> Result<()> {
		match op {
			FileOp::Paste(mut task) => {
				match fs::remove_file(&task.to).await {
					Err(e) if e.kind() != NotFound => Err(e)?,
					_ => {}
				}

				let mut it = copy_with_progress(&task.from, &task.to);
				while let Some(res) = it.recv().await {
					match res {
						Ok(0) => {
							if task.cut {
								fs::remove_file(&task.from).await.ok();
							}
							break;
						}
						Ok(n) => self.prog.send(TaskProg::Adv(task.id, 0, n))?,
						Err(e) if e.kind() == NotFound => {
							warn!("Paste task partially done: {:?}", task);
							break;
						}
						// Operation not permitted (os error 1)
						// Attribute not found (os error 93)
						Err(e)
							if task.retry < TASKS.bizarre_retry
								&& matches!(e.raw_os_error(), Some(1) | Some(93)) =>
						{
							self.log(task.id, format!("Paste task retry: {:?}", task))?;
							task.retry += 1;
							return Ok(self.macro_.send(FileOp::Paste(task).into(), LOW).await?);
						}
						Err(e) => Err(e)?,
					}
				}
				self.prog.send(TaskProg::Adv(task.id, 1, 0))?;
			}
			FileOp::Link(task) => {
				let meta = task.meta.as_ref().unwrap();

				let src = if task.resolve {
					match fs::read_link(&task.from).await {
						Ok(p) => Cow::Owned(p),
						Err(e) if e.kind() == NotFound => {
							self.log(task.id, format!("Link task partially done: {:?}", task))?;
							return Ok(self.prog.send(TaskProg::Adv(task.id, 1, meta.len()))?);
						}
						Err(e) => Err(e)?,
					}
				} else {
					Cow::Borrowed(task.from.as_path())
				};

				let src = if task.relative {
					path_relative_to(&src, &fs::canonicalize(task.to.parent().unwrap()).await?)
				} else {
					src
				};

				match fs::remove_file(&task.to).await {
					Err(e) if e.kind() != NotFound => Err(e)?,
					_ => {
						#[cfg(unix)]
						{
							fs::symlink(src, &task.to).await?
						}
						#[cfg(windows)]
						{
							if meta.is_dir() {
								fs::symlink_dir(src, &task.to).await?
							} else {
								fs::symlink_file(src, &task.to).await?
							}
						}
					}
				}

				if task.delete {
					fs::remove_file(&task.from).await.ok();
				}
				self.prog.send(TaskProg::Adv(task.id, 1, meta.len()))?;
			}
			FileOp::Delete(task) => {
				if let Err(e) = fs::remove_file(&task.target).await {
					if e.kind() != NotFound && fs::symlink_metadata(&task.target).await.is_ok() {
						self.fail(task.id, format!("Delete task failed: {:?}, {e}", task))?;
						Err(e)?
					}
				}
				self.prog.send(TaskProg::Adv(task.id, 1, task.length))?
			}
			FileOp::Trash(task) => {
				#[cfg(target_os = "macos")]
				{
					use trash::{macos::{DeleteMethod, TrashContextExtMacos}, TrashContext};
					let mut ctx = TrashContext::default();
					ctx.set_delete_method(DeleteMethod::NsFileManager);
					ctx.delete(&task.target)?;
				}
				#[cfg(all(not(target_os = "macos"), not(target_os = "android")))]
				{
					trash::delete(&task.target)?;
				}
				self.prog.send(TaskProg::Adv(task.id, 1, task.length))?;
			}
		}
		Ok(())
	}

	pub async fn paste(&self, mut task: FileOpPaste) -> Result<()> {
		if task.cut {
			match fs::rename(&task.from, &task.to).await {
				Ok(_) => return self.succ(task.id),
				Err(e) if e.kind() == NotFound => return self.succ(task.id),
				_ => {}
			}
		}

		let meta = Self::metadata(&task.from, task.follow).await?;
		if !meta.is_dir() {
			let id = task.id;
			self.prog.send(TaskProg::New(id, meta.len()))?;

			if meta.is_file() {
				self.macro_.send(FileOp::Paste(task).into(), LOW).await?;
			} else if meta.is_symlink() {
				self.macro_.send(FileOp::Link(task.to_link(meta)).into(), NORMAL).await?;
			}
			return self.succ(id);
		}

		macro_rules! continue_unless_ok {
			($result:expr) => {
				match $result {
					Ok(v) => v,
					Err(e) => {
						self.prog.send(TaskProg::New(task.id, 0))?;
						self.fail(task.id, format!("An error occurred while pasting: {e}"))?;
						continue;
					}
				}
			};
		}

		let root = task.to.clone();
		let skip = task.from.components().count();
		let mut dirs = VecDeque::from([task.from]);

		while let Some(src) = dirs.pop_front() {
			let dest = root.join(src.components().skip(skip).collect::<PathBuf>());
			continue_unless_ok!(match fs::create_dir(&dest).await {
				Err(e) if e.kind() != AlreadyExists => Err(e),
				_ => Ok(()),
			});

			let mut it = continue_unless_ok!(fs::read_dir(&src).await);
			while let Ok(Some(entry)) = it.next_entry().await {
				let src = Url::from(entry.path());
				let meta = continue_unless_ok!(Self::metadata(&src, task.follow).await);

				if meta.is_dir() {
					dirs.push_back(src);
					continue;
				}

				task.to = dest.join(src.file_name().unwrap());
				task.from = src;
				self.prog.send(TaskProg::New(task.id, meta.len()))?;

				if meta.is_file() {
					self.macro_.send(FileOp::Paste(task.clone()).into(), LOW).await?;
				} else if meta.is_symlink() {
					self.macro_.send(FileOp::Link(task.to_link(meta)).into(), NORMAL).await?;
				}
			}
		}
		self.succ(task.id)
	}

	pub async fn link(&self, mut task: FileOpLink) -> Result<()> {
		let id = task.id;
		if task.meta.is_none() {
			task.meta = Some(fs::symlink_metadata(&task.from).await?);
		}

		self.prog.send(TaskProg::New(id, task.meta.as_ref().unwrap().len()))?;
		self.macro_.send(FileOp::Link(task).into(), NORMAL).await?;
		self.succ(id)
	}

	pub async fn delete(&self, mut task: FileOpDelete) -> Result<()> {
		let meta = fs::symlink_metadata(&task.target).await?;
		if !meta.is_dir() {
			let id = task.id;
			task.length = meta.len();
			self.prog.send(TaskProg::New(id, meta.len()))?;
			self.macro_.send(FileOp::Delete(task).into(), NORMAL).await?;
			return self.succ(id);
		}

		let mut dirs = VecDeque::from([task.target]);
		while let Some(target) = dirs.pop_front() {
			let mut it = match fs::read_dir(target).await {
				Ok(it) => it,
				Err(_) => continue,
			};

			while let Ok(Some(entry)) = it.next_entry().await {
				let meta = match entry.metadata().await {
					Ok(m) => m,
					Err(_) => continue,
				};

				if meta.is_dir() {
					dirs.push_front(Url::from(entry.path()));
					continue;
				}

				task.target = Url::from(entry.path());
				task.length = meta.len();
				self.prog.send(TaskProg::New(task.id, meta.len()))?;
				self.macro_.send(FileOp::Delete(task.clone()).into(), NORMAL).await?;
			}
		}
		self.succ(task.id)
	}

	pub async fn trash(&self, mut task: FileOpTrash) -> Result<()> {
		let id = task.id;
		task.length = calculate_size(&task.target).await;

		self.prog.send(TaskProg::New(id, task.length))?;
		self.macro_.send(FileOp::Trash(task).into(), LOW).await?;
		self.succ(id)
	}

	async fn metadata(path: &Path, follow: bool) -> io::Result<Metadata> {
		if !follow {
			return fs::symlink_metadata(path).await;
		}

		let meta = fs::metadata(path).await;
		if meta.is_ok() { meta } else { fs::symlink_metadata(path).await }
	}

	pub(crate) fn remove_empty_dirs(dir: &Path) -> BoxFuture<()> {
		async move {
			let mut it = match fs::read_dir(dir).await {
				Ok(it) => it,
				Err(_) => return,
			};

			while let Ok(Some(entry)) = it.next_entry().await {
				if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
					let path = entry.path();
					Self::remove_empty_dirs(&path).await;
					fs::remove_dir(path).await.ok();
				}
			}

			fs::remove_dir(dir).await.ok();
		}
		.boxed()
	}
}

impl File {
	#[inline]
	fn succ(&self, id: usize) -> Result<()> { Ok(self.prog.send(TaskProg::Succ(id))?) }

	#[inline]
	fn fail(&self, id: usize, reason: String) -> Result<()> {
		Ok(self.prog.send(TaskProg::Fail(id, reason))?)
	}

	#[inline]
	fn log(&self, id: usize, line: String) -> Result<()> {
		Ok(self.prog.send(TaskProg::Log(id, line))?)
	}
}

impl FileOpPaste {
	fn to_link(&self, meta: Metadata) -> FileOpLink {
		FileOpLink {
			id:       self.id,
			from:     self.from.clone(),
			to:       self.to.clone(),
			meta:     Some(meta),
			resolve:  true,
			relative: false,
			delete:   self.cut,
		}
	}
}
