//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//
use async_std::task::sleep;
use clap::{App, Arg, Values};
use std::time::Duration;
use zenoh::net::*;

#[async_std::main]
async fn main() {
    // initiate logging
    env_logger::init();

    let (config, path, value) = parse_args();

    println!("Opening session...");
    let session = open(config).await.unwrap();

    print!("Declaring Resource {}", path);
    let rid = session.declare_resource(&path.into()).await.unwrap();
    println!(" => RId {}", rid);

    println!("Declaring Publisher on {}", rid);
    let _publisher = session.declare_publisher(&rid.into()).await.unwrap();

    for idx in 0..std::u32::MAX {
        sleep(Duration::from_secs(1)).await;
        let buf = format!("[{:4}] {}", idx, value);
        println!("Writing Data ('{}': '{}')...", rid, buf);
        session
            .write(&rid.into(), buf.as_bytes().into())
            .await
            .unwrap();
    }
}

fn parse_args() -> (Properties, String, String) {
    let args = App::new("zenoh-net pub example")
        .arg(
            Arg::from_usage("-m, --mode=[MODE] 'The zenoh session mode.")
                .possible_values(&["peer", "client"])
                .default_value("peer"),
        )
        .arg(Arg::from_usage(
            "-e, --peer=[LOCATOR]...  'Peer locators used to initiate the zenoh session.'",
        ))
        .arg(Arg::from_usage(
            "-l, --listener=[LOCATOR]...   'Locators to listen on.'",
        ))
        .arg(
            Arg::from_usage("-p, --path=[PATH]        'The name of the resource to publish.'")
                .default_value("/demo/example/zenoh-rs-pub"),
        )
        .arg(
            Arg::from_usage("-v, --value=[VALUE]      'The value of the resource to publish.'")
                .default_value("Pub from Rust!"),
        )
        .get_matches();

    let mut config = config::empty();
    config.push((
        config::ZN_MODE_KEY,
        args.value_of("mode").unwrap().as_bytes().to_vec(),
    ));
    for peer in args
        .values_of("peer")
        .or_else(|| Some(Values::default()))
        .unwrap()
    {
        config.push((config::ZN_PEER_KEY, peer.as_bytes().to_vec()));
    }
    for listener in args
        .values_of("listener")
        .or_else(|| Some(Values::default()))
        .unwrap()
    {
        config.push((config::ZN_LISTENER_KEY, listener.as_bytes().to_vec()));
    }

    let path = args.value_of("path").unwrap();
    let value = args.value_of("value").unwrap();

    (config, path.to_string(), value.to_string())
}