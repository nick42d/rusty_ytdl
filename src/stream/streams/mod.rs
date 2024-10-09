#[cfg(feature = "live")]
mod live;
mod non_live;

use async_trait::async_trait;
use bytes::Bytes;

#[cfg(feature = "ffmpeg")]
use bytes::BytesMut;
use futures::Stream;

#[cfg(feature = "ffmpeg")]
use std::{process::Stdio, sync::Arc};

#[cfg(feature = "ffmpeg")]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{
        mpsc::{channel, Receiver},
        Mutex, Notify,
    },
    task::JoinHandle,
};

#[cfg(feature = "live")]
pub use live::{LiveStream, LiveStreamOptions};
pub use non_live::{NonLiveStream, NonLiveStreamOptions};

#[cfg(feature = "ffmpeg")]
use crate::constants::DEFAULT_HEADERS;
use crate::VideoError;

#[async_trait]
pub trait YoutubeStream {
    /// Stream a chunk of the [`Bytes`]
    ///
    /// When the bytes has been exhausted, this will return `None`.
    async fn chunk(&self) -> Result<Option<Bytes>, VideoError>;

    /// Content length of the stream
    ///
    /// If stream is [`LiveStream`] returns always `0`
    fn content_length(&self) -> usize {
        0
    }

    fn into_futures_stream(self) -> impl futures::Stream<Item = Result<Bytes, VideoError>>
    where
        Self: Sized,
    {
        // Second value of initialisation tuple represents if the previous iteration of
        // the stream errored. If so, stream will close, as no future iterations of
        // the stream are expected to return Ok.
        futures::stream::unfold((self, false), |(state, err)| async move {
            if err {
                return None;
            };
            let chunk = state.chunk().await;
            match chunk {
                // Return error value on this iteration, on the next iteration return None.
                Err(e) => Some((Err(e), (state, true))),
                // Happy path
                Ok(Some(bytes)) => Some((Ok(bytes), (state, false))),
                // YoutubeStream has closed.
                Ok(None) => None,
            }
        })
    }
}

pub enum YoutubeStreamEnum {
    #[cfg(feature = "live")]
    Live(live::LiveStream),
    NonLive(non_live::NonLiveStream),
}

pub enum YoutubeStreamState<S> {
    Init,
    Running(S),
    Finished,
}

#[cfg(feature = "ffmpeg")]
pub struct FFmpegStreamOptions {
    pub client: reqwest_middleware::ClientWithMiddleware,
    pub link: String,
    pub content_length: u64,
    pub dl_chunk_size: u64,
    pub start: u64,
    pub end: u64,
    pub ffmpeg_args: Vec<String>,
}

#[cfg(feature = "ffmpeg")]
pub(crate) struct FFmpegStream {
    pub refined_data_reciever: Option<Arc<Mutex<Receiver<Bytes>>>>,
    download_notify: Arc<Notify>,
    ffmpeg_child: Child,

    tasks: Vec<JoinHandle<Result<(), VideoError>>>,
}

#[cfg(feature = "ffmpeg")]
impl FFmpegStream {
    pub fn new(options: FFmpegStreamOptions) -> Result<Self, VideoError> {
        let (tx, mut rx) = channel::<Bytes>(16384);
        let (refined_tx, refined_rx) = channel::<Bytes>(16384);

        // Spawn FFmpeg process
        let mut ffmpeg_child = Command::new("ffmpeg")
            .args(&options.ffmpeg_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|x| VideoError::FFmpeg(x.to_string()))?;

        let mut stdin = ffmpeg_child.stdin.take().unwrap();
        let mut stdout = ffmpeg_child.stdout.take().unwrap();

        let read_stdout_task = tokio::spawn(async move {
            let mut buffer = vec![0u8; 16384];
            while let Ok(line) = stdout.read(&mut buffer).await {
                match line {
                    0 => {
                        break;
                    }
                    n => {
                        if let Err(_err) = refined_tx.send(Bytes::from(buffer[..n].to_vec())).await
                        {
                            return Err(VideoError::FFmpeg("channel closed".to_string()));
                            // Error or channel closed
                        };
                    }
                }
            }

            Ok(())
        });

        let write_stdin_task = tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                if let Err(err) = stdin.write_all(&data).await {
                    return Err(VideoError::FFmpeg(err.to_string())); // Error or
                                                                     // channel closed
                }
            }
            Ok(())
        });

        let download_notify = Arc::new(Notify::new());
        let download_notify_task = download_notify.clone();

        let download_task = tokio::spawn(async move {
            let mut end = options.end;
            let mut start = options.start;
            let content_length = options.content_length;
            let client = options.client;
            let link = options.link;
            let dl_chunk_size = options.dl_chunk_size;

            download_notify_task.notified().await;

            loop {
                // Nothing else remain send break to finish
                if end == 0 {
                    break;
                }

                if end >= content_length {
                    end = 0;
                }

                let mut headers = DEFAULT_HEADERS.clone();

                let range_end = if end == 0 {
                    "".to_string()
                } else {
                    end.to_string()
                };

                headers.insert(
                    reqwest::header::RANGE,
                    format!("bytes={}-{}", start, range_end).parse().unwrap(),
                );

                let mut response = client
                    .get(&link)
                    .headers(headers)
                    .send()
                    .await
                    .map_err(VideoError::ReqwestMiddleware)?
                    .error_for_status()
                    .map_err(VideoError::Reqwest)?;

                let mut buf: BytesMut = BytesMut::new();

                while let Some(chunk) = response.chunk().await.map_err(VideoError::Reqwest)? {
                    buf.extend(chunk);
                }

                if end != 0 {
                    start = end + 1;

                    end += dl_chunk_size;
                }

                tx.send(buf.into())
                    .await
                    .map_err(|x| VideoError::FFmpeg(x.to_string()))?;
            }

            Ok(())
        });

        Ok(Self {
            refined_data_reciever: Some(Arc::new(Mutex::new(refined_rx))),
            download_notify,
            ffmpeg_child,
            tasks: vec![download_task, write_stdin_task, read_stdout_task],
        })
    }

    pub fn start_download(&self) {
        self.download_notify.notify_one();
    }
}

#[cfg(feature = "ffmpeg")]
impl Drop for FFmpegStream {
    fn drop(&mut self) {
        // kill tasks if they are still running
        for handle in &self.tasks {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Video, VideoOptions};
    use futures::{pin_mut, Stream, StreamExt, TryStreamExt};

    #[tokio::test]
    async fn test_stream() {
        let options = VideoOptions {
            download_options: crate::DownloadOptions {
                dl_chunk_size: Some(100000),
            },
            filter: crate::VideoSearchOptions::Audio,
            ..Default::default()
        };
        let video = Video::new_with_options("FZ8BxMU3BYc", options).unwrap();
        let stream = video.stream().await.unwrap();
        let stream = match stream {
            crate::stream::YoutubeStreamEnum::Live(stream) => todo!(),
            crate::stream::YoutubeStreamEnum::NonLive(stream) => stream,
        };
        let song = stream
            .into_stream_test()
            .enumerate()
            .map(|(idx, chunk)| {
                println!("Received chunk {idx}");
                chunk
            })
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert!(!song.is_empty());
    }
    // #[tokio::test]
    // async fn test_video() {
    //     let video = Video::new("FZ8BxMU3BYc").unwrap();
    //     let stream = video.stream().await.unwrap();
    //     while let Some(_) = stream.chunk().await.unwrap() {
    //         println!("Got a chunk");
    //     }
    // }
}
