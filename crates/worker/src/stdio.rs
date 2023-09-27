use bytes::Bytes;
use std::io::Cursor;
use wasi_common::pipe::{ReadPipe, WritePipe};
use wasmtime_wasi::{preview2, WasiCtxBuilder};

/// A library to configure the stdio of the WASI context.
/// Note that currently, wws relies on stdin and stdout
/// to send and read data from the worker.
///
/// The stdin/stdout approach will change in the future with
/// a more performant and appropiate approach.
pub struct Stdio {
    /// Defines the stdin ReadPipe to send the data to the module
    pub stdin: Vec<u8>,
    /// Defines the stdout to extract the data from the module
    pub stdout: WritePipe<Cursor<Vec<u8>>>,
}

impl Stdio {
    /// Initialize the stdio. The stdin will contain the input data.
    pub fn new(input: &[u8]) -> Self {
        Self {
            stdin: Vec::from(input),
            stdout: WritePipe::new_in_memory(),
        }
    }

    pub fn configure_wasi_ctx<'a>(
        &'a self,
        builder_preview1: Option<&'a mut WasiCtxBuilder>,
        builder_preview2: Option<&'a mut preview2::WasiCtxBuilder>,
    ) {
        if let Some(builder_preview1) = builder_preview1 {
            builder_preview1
                .stdin(Box::new(ReadPipe::new(Cursor::new(self.stdin.clone()))))
                .stdout(Box::new(self.stdout.clone()))
                .inherit_stderr();
        }
        if let Some(builder_preview2) = builder_preview2 {
            builder_preview2
                .stdin(
                    preview2::pipe::MemoryInputPipe::new(self.stdin.clone().into()),
                    preview2::IsATTY::No,
                )
                .inherit_stdout()
                .inherit_stderr();
        }
    }
}
