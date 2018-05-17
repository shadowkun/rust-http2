use futures::future::Future;
use futures::future;

use tokio_io::AsyncWrite;
use tokio_io::io::write_all;
use tokio_io::io::WriteHalf;

use common::types::Types;
use common::conn::ConnData;
use common::conn::ConnInner;
use common::stream::HttpStreamCommon;
use common::stream::HttpStreamData;
use rc_mut::RcMut;
use solicit::connection::HttpFrame;
use solicit::StreamId;
use solicit_async::HttpFuture;
use solicit::frame::FrameIR;

use data_or_headers_with_flag::DataOrHeadersWithFlag;

use error;
use ErrorCode;
use solicit::frame::RstStreamFrame;
use solicit::frame::GoawayFrame;
use solicit::frame::WindowUpdateFrame;
use solicit::frame::PingFrame;
use solicit::frame::SettingsFrame;


pub enum DirectlyToNetworkFrame {
    RstStream(RstStreamFrame),
    GoAway(GoawayFrame),
    WindowUpdate(WindowUpdateFrame),
    Ping(PingFrame),
    Settings(SettingsFrame),
}

impl DirectlyToNetworkFrame {
    pub fn into_http_frame(self) -> HttpFrame {
        match self {
            DirectlyToNetworkFrame::RstStream(f) => f.into(),
            DirectlyToNetworkFrame::GoAway(f) => f.into(),
            DirectlyToNetworkFrame::WindowUpdate(f) => f.into(),
            DirectlyToNetworkFrame::Ping(f) => f.into(),
            DirectlyToNetworkFrame::Settings(f) => f.into(),
        }
    }
}


pub struct WriteLoop<I, T>
    where
        I : AsyncWrite + 'static,
        T : Types,
        ConnData<T> : ConnInner,
        HttpStreamCommon<T> : HttpStreamData,
{
    pub write: WriteHalf<I>,
    pub inner: RcMut<ConnData<T>>,
}

impl<I, T> WriteLoop<I, T>
    where
        I : AsyncWrite + Send + 'static,
        T : Types,
        ConnData<T> : ConnInner<Types=T>,
        HttpStreamCommon<T> : HttpStreamData<Types=T>,
{
    fn write_all(self, buf: Vec<u8>) -> impl Future<Item=Self, Error=error::Error> {
        let WriteLoop { write, inner } = self;

        write_all(write, buf)
            .map(move |(write, _)| WriteLoop { write: write, inner: inner })
            .map_err(error::Error::from)
    }

    fn write_frame(self, frame: HttpFrame) -> impl Future<Item=Self, Error=error::Error> {
        debug!("send {:?}", frame);

        self.write_all(frame.serialize_into_vec())
    }

    fn with_inner<G, R>(&self, f: G) -> R
        where G : FnOnce(&mut ConnData<T>) -> R
    {
        self.inner.with(f)
    }

    pub fn send_outg_stream(self, stream_id: StreamId)
        -> impl Future<Item=Self, Error=error::Error>
    {
        let bytes = self.with_inner(|inner| {
            inner.pop_outg_all_for_stream_bytes(stream_id)
        });

        self.write_all(bytes)
    }

    fn send_outg_conn(self) -> impl Future<Item=Self, Error=error::Error> {
        let bytes = self.with_inner(|inner| {
            inner.pop_outg_all_for_conn_bytes()
        });

        self.write_all(bytes)
    }

    fn process_stream_end(self, stream_id: StreamId, error_code: ErrorCode) -> HttpFuture<Self> {
        let stream_id = self.inner.with(move |inner| {
            let stream = inner.streams.get_mut(stream_id);
            if let Some(mut stream) = stream {
                stream.stream().outgoing.close(error_code);
                Some(stream_id)
            } else {
                None
            }
        });
        if let Some(stream_id) = stream_id {
            Box::new(self.send_outg_stream(stream_id))
        } else {
            Box::new(future::finished(self))
        }
    }

    fn process_stream_enqueue(self, stream_id: StreamId, part: DataOrHeadersWithFlag) -> HttpFuture<Self> {
        let stream_id = self.inner.with(move |inner| {
            let stream = inner.streams.get_mut(stream_id);
            if let Some(mut stream) = stream {
                stream.stream().outgoing.push_back_part(part);
                Some(stream_id)
            } else {
                None
            }
        });
        if let Some(stream_id) = stream_id {
            Box::new(self.send_outg_stream(stream_id))
        } else {
            Box::new(future::finished(self))
        }
    }

    fn increase_in_window(self, stream_id: StreamId, increase: u32)
        -> impl Future<Item=Self, Error=error::Error>
    {
        let r = self.inner.with(move |inner| {
            inner.increase_in_window(stream_id, increase)
        });
        future::result(r.map(|()| self))
    }

    pub fn process_common(self, common: CommonToWriteMessage) -> HttpFuture<Self> {
        match common {
            CommonToWriteMessage::TryFlushStream(None) => {
                Box::new(self.send_outg_conn())
            },
            CommonToWriteMessage::TryFlushStream(Some(stream_id)) => {
                Box::new(self.send_outg_stream(stream_id))
            },
            CommonToWriteMessage::Frame(frame) => {
                Box::new(self.write_frame(frame.into_http_frame()))
            },
            CommonToWriteMessage::StreamEnd(stream_id, error_code) => {
                self.process_stream_end(stream_id, error_code)
            },
            CommonToWriteMessage::StreamEnqueue(stream_id, part) => {
                self.process_stream_enqueue(stream_id, part)
            },
            CommonToWriteMessage::IncreaseInWindow(stream_id, increase) => {
                Box::new(self.increase_in_window(stream_id, increase))
            },
            CommonToWriteMessage::CloseConn => {
                Box::new(future::err(error::Error::Other("close connection")))
            }
        }
    }
}


// Message sent to write loop.
// Processed while write loop is not handling network I/O.
pub enum CommonToWriteMessage {
    TryFlushStream(Option<StreamId>), // flush stream when window increased or new data added
    IncreaseInWindow(StreamId, u32),
    Frame(DirectlyToNetworkFrame),    // write frame immediately to the network
    StreamEnqueue(StreamId, DataOrHeadersWithFlag),
    StreamEnd(StreamId, ErrorCode),   // send when user provided handler completed the stream
    CloseConn,
}
