// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";

/// A live 1:1 call: the peer userref + its LiveKit room and state.
export interface ActiveCall {
  peer: string;
  room: string;
  state: string;
}
/// An incoming call ring.
export interface IncomingCall {
  from: string;
  room: string;
}
/// A group DM: an optional name + member userrefs.
export interface GroupInfo {
  name?: string;
  members: string[];
}

/**
 * The social layer (federation-able): friends, group DMs, and calls. Users are
 * `account@network` userrefs, resolved through the Account identity map at the
 * UI boundary (e.g. `<Avatar>`). Replaces the parallel `friends` / `groups` /
 * `groupCallRoster` records + the call `$state` fields that lived in
 * `+page.svelte`.
 */
export class Social {
  /// Friend userref → relationship ("friends" | "incoming" | "outgoing").
  readonly friends = new SvelteMap<string, string>();
  /// Group DM id (`&<ulid>`) → group.
  readonly groups = new SvelteMap<string, GroupInfo>();
  /// Group call id → the userrefs currently in the call.
  readonly groupCallRoster = new SvelteMap<string, string[]>();

  /// Incoming 1:1 call ring, if any.
  incomingCall = $state<IncomingCall | null>(null);
  /// The active 1:1 call, if any.
  activeCall = $state<ActiveCall | null>(null);
  /// The group call I'm currently in, if any.
  activeGroupCall = $state<string | null>(null);
}
