
#![allow(dead_code, unused_variables, unused_imports)]

extern crate libc;
extern crate rand;
extern crate chrono;
extern crate futures;
extern crate futures_cpupool;
extern crate tokio_core;
extern crate ocl;
extern crate ocl_extras as extras;
#[macro_use] extern crate colorify;

use std::thread;
use std::sync::mpsc;
use std::collections::HashMap;
use rand::{Rng, XorShiftRng};
use rand::distributions::{IndependentSample, Range as RandRange};
use chrono::{Duration, DateTime, Local};
use futures::{stream, Future, Sink, Stream, Join};
// use futures::sync::mpsc::{self, Sender};
use futures_cpupool::{CpuPool, CpuFuture, Builder};
use ocl::{Platform, Device, Context, Queue, Program, Kernel, OclPrm,
    Event, EventList, FutureMemMap, Buffer};
use ocl::flags::{MemFlags, MapFlags, CommandQueueProperties};
use ocl::aliases::ClInt4;
use ocl::async::{Error as AsyncError};
use extras::{SubBufferPool, CommandGraph, Command, CommandDetails, KernelArgBuffer};

// const INITIAL_BUFFER_LEN: u32 = 2 << 24; // 512MiB of ClInt4
// const SUB_BUF_MIN_LEN: u32 = 2 << 15; // 1MiB of ClInt4
// const SUB_BUF_MAX_LEN: u32 = 2 << 19; // 16MiB of ClInt4

const WORK_SIZE: usize = 2 << 14;
const TASK_ITERS: usize = 128;

pub static KERN_SRC: &'static str = r#"
    __kernel void add(
        __global int4* in,
        __private int4 values,
        __global int4* out)
    {
        uint idx = get_global_id(0);
        out[idx] = in[idx] + values;
    }
"#;


pub fn fmt_duration(duration: Duration) -> String {
    let el_sec = duration.num_seconds();
    let el_ms = duration.num_milliseconds() - (el_sec * 1000);
    format!("{}.{} seconds", el_sec, el_ms)
}


pub fn main() {
    let start_time = extras::now();

    let platform = Platform::default();
    println!("Platform: {}", platform.name());
    let device = Device::first(platform);
    println!("Device: {} {}", device.vendor(), device.name());

    let context = Context::builder()
        .platform(platform)
        .devices(device)
        .build().unwrap();

    let queue_flags = Some(CommandQueueProperties::new().out_of_order());
    let mut queues: Vec<_> = (0..3).map(|_| Queue::new(&context, device, queue_flags).unwrap()).collect();

    let write_buf_flags = MemFlags::new().read_only().host_write_only();
    let read_buf_flags = MemFlags::new().write_only().host_read_only();

    // Create write and read buffers:
    let write_buf: Buffer<ClInt4> = Buffer::builder()
        .queue(queues.pop().unwrap())
        .flags(write_buf_flags)
        .dims(WORK_SIZE)
        .build().unwrap();

    let read_buf: Buffer<ClInt4> = Buffer::builder()
        .queue(queues.pop().unwrap())
        .flags(read_buf_flags)
        .dims(WORK_SIZE)
        .build().unwrap();

    // Create program and kernel:
    let program = Program::builder()
        .devices(device)
        .src(KERN_SRC)
        .build(&context).unwrap();

    let kern = Kernel::new("add", &program).unwrap()
        .queue(queues.pop().unwrap())
        .gws(WORK_SIZE)
        .arg_buf(&write_buf)
        .arg_vec(ClInt4(100, 100, 100, 100))
        .arg_buf(&read_buf);

    // // A channel with room to keep 24 tasks in-flight.
    // let (mut tx, rx) = mpsc::sync_channel(24);

    // // Create a thread to handle the stream of work:
    // let completion_thread = thread::spawn(|| {
    //     let mut in_flight = HashMap::with_capacity(36);

    //     match rx.recv().unwrap() {
    //         Some(join) => {
    //             in_flight
    //             continue,
    //         },
    //         None => break,
    //     }

    //     println!("All sent futures complete.");
    // });


    // Thread pool for offloaded tasks.
    let thread_pool = CpuPool::new_num_cpus();

    // (0) INIT: Fill buffer with -999's just to ensure the upcoming
    // write misses nothing:
    let mut start_event = Event::empty();
    // write_buf.cmd().fill(ClInt4(0, 0, 0, 0), None).enew(&mut start_event).enq().unwrap();

    read_buf.cmd().fill(ClInt4(99, 5, 0, 5), None).enew(&mut start_event).enq().unwrap();

    start_event.wait_for().unwrap();


    for task_iter in 0..TASK_ITERS {
        // (1) WRITE: Map the buffer and write 50's to the entire buffer, then
        // unmap to 'flush' data to the device:
        let mut future_write_data = write_buf.cmd().map()
            .flags(MapFlags::new().write_invalidate_region())
            .ewait(&start_event)
            .enq_async().unwrap();

        let write_unmap_event = future_write_data.create_unmap_event().unwrap().clone();

        let write = future_write_data.and_then(move |mut data| {
            for val in data.iter_mut() {
                *val = ClInt4(50, 50, 50, 50);
            }

            println!("Mapped write complete (iter: {}). ", task_iter);

            Ok(task_iter)
        });

        // let spawned_write = thread_pool.spawn(write);

        // [DEBUG]:
        let spawned_write = thread_pool.spawn(write).wait().unwrap();

        // (2) KERNEL: Run kernel: Add 100 to everything (total should now be 150):
        let mut kern_event = Event::empty();

        kern.cmd()
            .enew(&mut kern_event)
            .ewait(&write_unmap_event)
            .enq().unwrap();

        // [DEBUG]:
        kern_event.wait_for().unwrap();

        // (3) READ: Read results and verify that the write and kernel have
        // both completed successfully:
        let mut future_read_data = read_buf.cmd().map()
            .flags(MapFlags::new().read())
            .ewait(&kern_event)
            .enq_async().unwrap();

        // Put the read unmap event into the `start_event` for the next iter.
        let start_event = future_read_data.create_unmap_event().unwrap().clone();

        let read = future_read_data.and_then(move |data| {
                let mut val_count = 0usize;

                for (idx, val) in data.iter().enumerate() {
                    let correct_val = ClInt4(150, 150, 150, 150);
                    if *val != correct_val {
                        return Err(format!("Result value mismatch: {:?} != {:?} @ [{}]", val, correct_val, idx).into());
                    }
                    val_count += 1;
                }

                println!("Mapped read and verify complete (task: {}). ", task_iter);

                Ok(val_count)
            });

        let spawned_read = thread_pool.spawn(read);

        // [DEBUG]:
        spawned_read.wait().unwrap();

        // let join = spawned_write.join(spawned_read);

        // tx.send(Some(join)).unwrap();
        // join.wait().unwrap();
    }

    // tx.send(None);

    // completion_thread.join().unwrap();
}