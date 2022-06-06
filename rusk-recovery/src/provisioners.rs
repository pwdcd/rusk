// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use dusk_bls12_381_sign::PublicKey;
use dusk_bytes::DeserializableSlice;
use lazy_static::lazy_static;

lazy_static! {
    pub static ref PROVISIONERS: [PublicKey; 5] = [
        parse_key(include_bytes!("../provisioners/node_0.cpk")),
        parse_key(include_bytes!("../provisioners/node_1.cpk")),
        parse_key(include_bytes!("../provisioners/node_2.cpk")),
        parse_key(include_bytes!("../provisioners/node_3.cpk")),
        parse_key(include_bytes!("../provisioners/node_4.cpk")),
    ];
    pub static ref DUSK_KEY: PublicKey =
        parse_key(include_bytes!("../dusk.cpk"));
}

fn parse_key(key_bytes: &[u8]) -> PublicKey {
    PublicKey::from_slice(key_bytes).expect("Genesis consensus key to be valid")
}
