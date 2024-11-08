// @ts-nocheck
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

import * as ProtocolDriver from "../src/protocol-driver/mod.js";
import { ProfileGenerator, Profile } from "./profile.js";

import {
  Transfer,
  UnshieldTransfer,
  ShieldTransfer,
} from "../src/transaction.js";

class BookEntry {
  constructor(bookkeeper, profile) {
    this.bookkeeper = bookkeeper;
    this.profile = profile;

    Object.freeze(this);
  }

  async balance(type) {
    return this.bookkeeper.balance(this.profile[type]);
  }

  transfer(amount) {
    return new Transfer(this).amount(amount);
  }

  unshield(amount) {
    return new UnshieldTransfer(this).amount(amount);
  }

  shield(amount) {
    return new ShieldTransfer(this).amount(amount);
  }
}

export class Bookkeeper {
  #treasury;

  constructor(treasury) {
    this.#treasury = treasury;
  }

  async balance(identifier) {
    const type = ProfileGenerator.typeOf(String(identifier));
    switch (type) {
      case "account":
        return await this.#treasury.account(identifier);
      case "address":
        const notes = await this.#treasury.address(identifier);
        const seed = await ProfileGenerator.seedFrom(identifier);
        const index = +identifier;

        return ProtocolDriver.balance(seed, index, notes);
    }
  }

  async pick(identifier, amount) {
    const notes = await this.#treasury.address(identifier);
    const seed = await ProfileGenerator.seedFrom(identifier);
    const index = +identifier;

    const { spendable } = ProtocolDriver.balance(seed, index, notes);

    if (spendable < amount) {
      throw new Error("Insufficient funds");
    }

    return ProtocolDriver.pickNotes(identifier, notes, amount);
  }

  as(profile) {
    if (!(profile instanceof Profile)) {
      throw new TypeError(`${profile} is not a Profile instance`);
    }

    return new BookEntry(this, profile);
  }
}
