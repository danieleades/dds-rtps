/// Interoperability test program for RustDDS library
use log::{debug, error, trace, LevelFilter};
use log4rs::{append::console::ConsoleAppender, config::Appender, config::Root, Config};

use rustdds::dds::data_types::DDSDuration;
use rustdds::dds::data_types::TopicKind;
use rustdds::dds::qos::policy::{Deadline, Durability, History, Reliability};
use rustdds::dds::qos::QosPolicyBuilder;
use rustdds::dds::statusevents::StatusEvented;
use rustdds::dds::traits::Keyed;
use rustdds::dds::traits::TopicDescription;
use rustdds::dds::DomainParticipant;
use serde::{Deserialize, Serialize};

use clap::ArgEnum;
use clap::Clap;
use clap::{App, Arg}; // command line argument processing

use mio::*; // polling
use mio_extras::channel; // pollable channel

use std::io;

use rand::prelude::*;

use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone)]
struct Shape {
    color: String,
    x: i32,
    y: i32,
    shapesize: i32,
}

impl Keyed for Shape {
    type K = String;
    fn get_key(&self) -> String {
        self.color.clone()
    }
}

const DA_WIDTH: i32 = 240;
const DA_HEIGHT: i32 = 270;

const STOP_PROGRAM: Token = Token(0);
const READER_READY: Token = Token(1);
const READER_STATUS_READY: Token = Token(2);
const WRITER_STATUS_READY: Token = Token(3);

#[derive(Clap)]
struct Args {
    /// Sets the DDS domain id number
    #[clap(short = 'd', long, default_value_t, value_name = "id")]
    domain_id: u16,

    /// Sets the topic name
    #[clap(short, long, value_name = "name", default_value = "Square")]
    topic: String,

    /// Color to publish (or filter)
    #[clap(short, long, default_value = "BLUE")]
    color: String,

    /// Set durability
    #[clap(arg_enum, short = 'D', long, default_value = "v")]
    durability: DurabilityArg,

    #[clap(subcommand)]
    command: Command,

    #[clap(arg_enum, long, short)]
    reliability: Option<ReliabilityArg>,

    /// Keep history depth
    #[clap(short = 'k', long, default_value_t)]
    history_depth: i32,

    /// Set a 'deadline' with interval (seconds)
    #[clap(short = 'f', long, value_name = "interval")]
    deadline: Option<f64>,

    /// "Set a 'partition' string"
    #[clap(short, long)]
    partition: Option<String>,

    /// Apply 'time based filter' with interval (seconds)
    #[clap(short, long)]
    interval: Option<f64>,

    /// Set ownership strength [-1: SHARED]
    #[clap(short, long, value_name = "strength")]
    ownership_strength: Option<u16>,
}

#[derive(ArgEnum)]
enum DurabilityArg {
    V,
    L,
    T,
    P,
}

#[derive(Clap)]
enum Command {
    /// Act as publisher
    Publish,

    /// Act as subscriber
    Subscribe,
}

#[derive(ArgEnum)]
enum ReliabilityArg {
    Reliable,
    BestEffort,
}

impl Args {
    pub fn reliability(&self) -> Reliability {
        match self.reliability {
            Some(ReliabilityArg::Reliable) => Reliability::Reliable {
                max_blocking_time: DDSDuration::DURATION_ZERO,
            },
            Some(ReliabilityArg::BestEffort) | None => Reliability::BestEffort,
        }
    }

    pub fn durability(&self) -> Durability {
        match self.durability {
            DurabilityArg::V => Durability::Volatile,
            DurabilityArg::L => Durability::TransientLocal,
            DurabilityArg::T => Durability::Transient,
            DurabilityArg::P => Durability::Persistent,
        }
    }

    pub fn history_depth(&self) -> History {
        match self.history_depth {
            x if x < 0 => History::KeepAll,
            x => History::KeepLast { depth: x },
        }
    }

    pub fn deadline(&self) -> Option<Deadline> {
        self.deadline
            .map(|d| Deadline(DDSDuration::from_frac_seconds(d)))
    }

    pub fn domain_participant(&self) -> Result<DomainParticipant, rustdds::dds::error::Error> {
        DomainParticipant::new(self.domain_id)
    }
}

fn main() {
    // initialize logging, preferably from config file
    log4rs::init_file("logging-config.yaml", Default::default()).unwrap_or_else(|e| {
        match e.downcast_ref::<io::Error>() {
            // Config file did not work. If it is a simple "No such file or directory", then
            // substitute some default config.
            Some(os_err) if os_err.kind() == io::ErrorKind::NotFound => {
                println!("No config file.");
                let stdout = ConsoleAppender::builder().build();
                let conf = Config::builder()
                    .appender(Appender::builder().build("stdout", Box::new(stdout)))
                    .build(Root::builder().appender("stdout").build(LevelFilter::Error))
                    .unwrap();
                log4rs::init_config(conf).unwrap();
            }
            // Give up.
            other_error => panic!("Config problem: {:?}", other_error),
        }
    });

    let args = Args::parse();

    let domain_participant = args
        .domain_participant()
        .unwrap_or_else(|e| panic!("DomainParticipant construction failed: {:?}", e));

    let mut qos_b = QosPolicyBuilder::new()
        .reliability(args.reliability())
        .durability(args.durability())
        .history(args.history_depth());

    if let Some(deadline) = args.deadline() {
        qos_b = qos_b.deadline(deadline);
    }

    assert!(
        args.partition.is_none(),
        "QoS policy Partition is not yet implemented."
    );
    assert!(
        args.interval.is_none(),
        "QoS policy Time Based Filter is not yet implemented."
    );
    assert!(
        args.ownership_strength.is_none(),
        "QoS policy Ownership Strength is not yet implemented."
    );

    let qos = qos_b.build();

    let topic = domain_participant
        .create_topic(&args.topic, "ShapeType", &qos, TopicKind::WithKey)
        .unwrap_or_else(|e| panic!("create_topic failed: {:?}", e));
    println!(
        "Topic name is {}. Type is {}.",
        topic.get_name(),
        topic.get_type().name()
    );

    // Set Ctrl-C handler
    let (stop_sender, stop_receiver) = channel::channel();
    ctrlc::set_handler(move || {
        stop_sender.send(()).unwrap_or(())
        // ignore errors, as we are quitting anyway
    })
    .expect("Error setting Ctrl-C handler");
    println!("Press Ctrl-C to quit.");

    let poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(4);

    poll.register(
        &stop_receiver,
        STOP_PROGRAM,
        Ready::readable(),
        PollOpt::edge(),
    )
    .unwrap();

    match args.command {
        Command::Publish => todo!(),
        Command::Subscribe => todo!(),
    }

    /*   let mut writer_opt =
      if is_publisher {
        debug!("Publisher");
        let publisher = domain_participant.create_publisher(&qos).unwrap();
        let mut writer = publisher
              .create_datawriter_CDR::<Shape>( topic.clone(), None) // None = get qos policy from publisher
              .unwrap();
        poll.register(writer.as_status_evented(), WRITER_STATUS_READY, Ready::readable(), PollOpt::edge())
          .unwrap();
        Some(writer)
      } else { None };

    let mut reader_opt =
      if is_subscriber {
        debug!("Subscriber");
        let subscriber = domain_participant.create_subscriber(&qos).unwrap();
        let mut reader = subscriber
          .create_datareader_CDR::<Shape>( topic.clone(), Some(qos) )
          .unwrap();
        poll.register(&reader, READER_READY, Ready::readable(),PollOpt::edge())
          .unwrap();
        poll.register(reader.as_status_evented(), READER_STATUS_READY, Ready::readable(), PollOpt::edge())
          .unwrap();
        debug!("Created DataReader");
        Some(reader)
      } else { None };

    let mut shape_sample = Shape { color: color.to_string(), x: 0, y: 0, shapesize: 21 };
    let mut random_gen = thread_rng();
    // a bit complicated lottery to ensure we do not end up with zero velocity.
    let mut x_vel = if random() { random_gen.gen_range(1..5) } else { random_gen.gen_range(-5..-1) };
    let mut y_vel = if random() { random_gen.gen_range(1..5) } else { random_gen.gen_range(-5..-1) };

    let mut last_write = Instant::now();

      loop {
          poll
              .poll(&mut events, Some(Duration::from_millis(200)))
              .unwrap();
          for event in &events {
              match event.token() {
                  STOP_PROGRAM => {
                      match stop_receiver.try_recv() {
                          Ok(_) => {
                            println!("Done.");
                            return
                          }
                          Err(_) => { /* Can this even happen? */ }
                      }
                  }
          READER_READY => {
              match reader_opt {
                Some(ref mut reader) => {
                  loop {
                    trace!("DataReader triggered");
                    match reader.take_next_sample() {
                      Ok(Some(sample)) =>
                        match sample.into_value() {
                          Ok(sample) =>
                            println!("{:10.10} {:10.10} {:3.3} {:3.3} [{}]",
                                      topic.get_name(),
                                      sample.color,
                                      sample.x,
                                      sample.y,
                                      sample.shapesize,
                                      ),
                          Err(key) =>
                            println!("Disposed key {:?}", key),
                          },
                      Ok(None) => break, // no more data
                      Err(e) => println!("DataReader error {:?}", e),
                    } // match
                  }
                }
                None => { error!("Where is my reader?"); }
              }
            }
          READER_STATUS_READY => {
            match reader_opt {
              Some(ref mut reader) => {
                while let Some(status) = reader.try_recv_status() {
                  println!("DataReader status: {:?}", status);
                }
              }
              None => { error!("Where is my reader?"); }
            }
          }

                  WRITER_STATUS_READY => {
            match writer_opt {
              Some(ref mut writer) => {
                          while let Some(status) = writer.try_recv_status() {
                              println!("DataWriter status: {:?}", status);
                          }
              }
              None => { error!("Where is my writer?"); }
            }
                  }
                  other_token => {
                      println!("Polled event is {:?}. WTF?", other_token);
                  }
              }
          }

      let r = move_shape(shape_sample,x_vel,y_vel);
      shape_sample = r.0;
      x_vel = r.1;
      y_vel = r.2;

      // write to DDS
      trace!("Writing shape color {}", &color);
      match writer_opt {
        Some(ref mut writer) => {
          let now = Instant::now();
          if last_write + Duration::from_millis(200) < now {
            writer.write( shape_sample.clone() , None)
              .unwrap_or_else(|e| error!("DataWriter write failed: {:?}",e));
            last_write = now;
          }
        }
        None => {
          if is_publisher {
            error!("Where is my writer?");
          } else { /* never mind */ }
        }
      }

      } // loop */
}

fn move_shape(shape: Shape, xv: i32, yv: i32) -> (Shape, i32, i32) {
    let half_size = shape.shapesize / 2 + 1;
    let mut x = shape.x + xv;
    let mut y = shape.y + yv;

    let mut xv_new = xv;
    let mut yv_new = yv;

    if x < half_size {
        x = half_size;
        xv_new = -xv;
    }
    if x > DA_WIDTH - half_size {
        x = DA_WIDTH - half_size;
        xv_new = -xv;
    }
    if y < half_size {
        y = half_size;
        yv_new = -yv;
    }
    if y > DA_HEIGHT - half_size {
        y = DA_HEIGHT - half_size;
        yv_new = -yv;
    }
    (
        Shape {
            color: shape.color,
            x,
            y,
            shapesize: shape.shapesize,
        },
        xv_new,
        yv_new,
    )
}
