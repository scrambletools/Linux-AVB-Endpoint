# avb-endpointd: this computer's entities, for any app

Status: reviewed by triib (2026-11-08). Steps 1 and 2 began in
scrambletools/avb, where avb-endpointd 0.1.0 and 0.1.1 were released,
and moved here with their history (2026-11-09). Step 2 is released
from here as avb-endpointd 0.2.0 and avb-pipewire 0.1.1, tagged
avb-endpointd-v0.2.0 (2026-11-10); triib runs step 1.

## Why

A person should be able to put this computer on an AVB network as a
talker that any app plays into, as it would into speakers, without plaa
or triib running. Today neither can do that:

- plaa's entity lives inside plaa. Its one stream output sends a copy
  of what plaa itself plays, and only while plaa runs.
- triib's entities run in triib-endpointd, which outlives triib's window
  and already gives each stream a PipeWire device apps play into or
  record from. But only triib starts it, with nothing at login, and it is
  triib's: plaa can't add an entity to it.

Two programs running entities on one interface also each carry the same
machinery: an MRP participant, with plaa's sharing, handoff and
step-aside logic so that the two don't withdraw each other's
declarations; a raw socket helper (plaa-netraw, triib-netraw); a link
unit (plaa-link@, triib-link@); and starting gptpd.

avb-endpointd is triib-endpointd moved out of triib and made
neutral: one daemon per interface in the person's session, running every
entity on the computer, whichever app made it. Each stream's audio comes
from or goes to a PipeWire device. plaa and triib become two ways to
configure it.

## What it does, and what it leaves to the programs

It does, as triib-endpointd does now:

- run the computer's entities on one interface from a configuration
  file: ADP, the AEM responder, ACMP, MAAP, MSRP and MVRP in one shared
  participant, CVU SRP for AVB Lite, and gPTP from gptpd;
- send and receive each entity's streams with avb-stream's threads,
  under RealtimeKit, as now (see "The data path");
- give each stream output a PipeWire playback device, and each stream
  input a recording device. A stream can instead use a cpal device, a
  tone or silence;
- keep what controllers change (names, formats, transit times, clock
  sources, mappings, bindings) in the configuration file;
- start gptpd and the link unit on the interface, as both programs do
  now.

It leaves to the programs:

- controllers. plaa's and triib's controllers stay in their apps, as
  neither reserves streams;
- choosing what each app plays into a device or records from it. To the
  daemon, plaa is one more app playing into a device.

## Library and daemon

triib builds from crates.io on Linux, macOS and Windows, and uses the
daemon's types on all three: the configuration, status and meters, and
the entity IDs, in its host, meter, preset and network views. So
`avb-endpointd` is published in two halves:

- **A library** that builds everywhere: the configuration file's types,
  with load and save under the file lock, the model files, the status
  and meters readers, the paths, the unit's start, disable and state,
  and `entity_id`. triib-store's load, save and paths that the daemon
  uses move with it.
- **The daemon**, Linux only, behind a feature.

The daemon and the apps no longer ship together, so:

- each app's package needs a minimum avb-endpointd version;
- readers take values they don't know as "other", as status's Setup
  does now.

## Shape

### The unit

- `avb-endpointd@<interface>.service` is a systemd user unit template
  in `/usr/lib/systemd/user`. `ExecStart` passes `%i`, the interface's
  name as it is, as gptpd@ and the link units do: `%I` would unescape a
  dash into a slash. The daemon keeps checking that the name is an
  interface it can use. It runs in
  the person's session, so its devices are in that session's PipeWire.
- **Ordering:**
  - `WantedBy=` and `PartOf=graphical-session.target`. default.target
    would also start it under linger and on logins that are only SSH,
    and there polkit refuses gptpd@ and the link units, and RealtimeKit
    refuses real-time priority. A desktop that never reaches the
    graphical session target doesn't start it at login; an app still
    starts it.
  - `After=pipewire.service` and `PartOf=pipewire.service`, so a
    PipeWire restart restarts the daemon and its devices. The entities
    leave the network for that moment. Later, avb-pipewire reconnects
    after a core error instead.
- **Restarts:**
  - `Restart=on-failure` replaces triib's bring_back.
  - Exits that aren't crashes have their own exit status, listed in
    `RestartPreventExitStatus=`, so they don't loop into the start
    limit: another person's daemon holds the interface, no such
    interface, or a configuration file that doesn't parse.
  - Apps run `systemctl --user reset-failed` for the instance before
    starting it.
  - `RestartSteps=` and `RestartMaxDelaySec=` need systemd 254 (Debian
    12 has 252 and warns and ignores them), so the unit uses only a
    fixed `RestartSec=`.
- **Process:**
  - `KillMode=mixed`, so the daemon ends its own helpers.
  - No hardening that implies NoNewPrivileges, which would strip
    avb-netraw's file capability. Started by systemd, the daemon always
    gets the helper with its capability. A triib started from a
    NoNewPrivileges process doesn't today.
  - It logs to the journal instead of `endpointd.log`.
- **Who starts it:**
  - An app enables and starts it when a person puts this computer on
    the network on an interface, or adds an entity there, with
    `unit::start`: `reset-failed`, then `enable --now`. A user unit
    needs no polkit. Enabled, it starts at login with no app running.
    `unit::state` tells a refusal, a missing interface and a file that
    doesn't parse apart by the exit status.
  - Whenever it runs, it runs the default entity (see "The default
    entity"), so it doesn't end by itself; `unit::disable` stops it and
    its starting at login, taking every entity on the interface off the
    network.
  - With no configuration file at all, it imports triib's (see
    "Migration"), and adds the default entity.

### One interface, one person

- One daemon per interface across people, claimed by the abstract
  socket `@avb-endpointd/<interface>`. A second person's daemon exits
  with its refusal status, saying whose holds the interface.
- While old versions can exist, the daemon binds both
  `@avb-endpointd/<interface>` and `@triib-endpointd/<interface>`. A new
  daemon then won't start beside an old triib-endpointd, and an old one
  won't start beside a new daemon. triib's `elsewhere()` checks both
  names.
- The per-person lock is per interface:
  `$XDG_RUNTIME_DIR/avb-endpointd/<interface>.lock`. Today's single lock
  per person would turn away a second instance of the template.

### Privileges, gPTP and the link

- **No capability on the daemon.** `avb-netraw` holds `cap_net_raw+ep`,
  set at install, and hands the daemon AF_PACKET sockets over a
  socketpair. It replaces plaa-netraw and triib-netraw, which run the
  same broker today. Both apps' controllers use it too.
- **Real-time:** RealtimeKit gives the stream threads round robin
  priority 15, as now.
- **gptpd:** the daemon reads `/run/gptpd/<interface>.sock` and
  `.time`. Where none answers, it starts `gptpd@<interface>.service`
  under gptpd's polkit rule, as both programs do now. polkit passes it
  from the user manager's session slice, as it does triib-endpointd now.
- **Link units:** `avb-link@<interface>.service` and
  `avb-link-reset@<interface>.service` replace plaa-link@, triib-link@
  and triib-link-reset@, with a polkit rule for the active local
  session.
  - At step 1, avb-link@ carries what triib-link@ carries then.
  - The first transmit queue gets fq_codel, whatever the system's
    default. The bench found fq there made frames arrive late, stamped
    or not; departure times wait for a card that keeps them itself.
  - The daemon retries the link unit after a refusal, as it retries
    gptpd's start. Today start_link runs once.
  - While the three names can be live, an active avb-, triib- or
    plaa-link@ on the interface counts as already run. Running it again
    would bounce a link whose EEE was on.

## Files

### Configuration

`$XDG_CONFIG_HOME/avb-endpointd/<interface>.toml`, one per interface.
It is triib's `entities.toml` with these changes:

- The interface comes from the file's name. `interface` and the restart
  when it changes go.
- Each entity names its model (below). One is the default entity
  (below); any app, or a person by hand, adds and removes the others,
  which belong to no app.
- Every entity carries an explicit `instance`. Today an entry without
  one takes its place in the list, so one app removing an entry above
  another's would change the other's entity ID. The daemon writes the
  instances in as it imports or first reads a file.

Rules for changing it:

- **Locking:** every change, by either app or the daemon, goes
  through `config::edit`, which holds an flock on
  `<interface>.toml.lock` beside the file from reading it to writing
  it back, and only for that edit. It writes every instance down and
  writes only where something changed. Reading alone takes no lock, as
  every write replaces the file whole. This is not the daemon's
  instance lock in `$XDG_RUNTIME_DIR`, which it holds for its whole
  life. The edit lock is a separate file because the write replaces the
  configuration file's inode. Without it, an app writing a stale copy
  loses another writer's change.
- **Writes** are by atomic rename.
- **Change detection:** the daemon watches the folder with inotify,
  which catches writes within one timestamp tick that the mtime check
  misses. It keeps the two-second check as a fallback.
- It applies entity changes in place, as now.

### Status and meters

- **Status:** `$XDG_RUNTIME_DIR/avb-endpointd/<interface>.toml`,
  rewritten every two seconds, as triib's now:
  - the process, gptpd's state and the clock;
  - each entity, with its model and whether it shows the generic model,
    whether it is the default, and whether it is held off the network
    because another program here advertises its entity ID, and each
    stream;
  - each stream's state as plaa shows it now: sending or waiting for a
    listener, listening, bound or unbound, and while it doesn't run,
    `blocked` with why: the link unit, no link, gptpd starting, no
    gptpd, gptpd refused, no PTP hardware clock, or no virtual device.
    Another person's daemon holding the interface shows in the unit's
    state instead, as this person's daemon then doesn't run.
- **Meters:** `$XDG_RUNTIME_DIR/avb-endpointd/<interface>.meters`,
  written every 50 ms while an app touches `<interface>.meters-wanted`,
  as triib's now.

### MAAP and sharing

- **MAAP:** the daemon takes one block of 32 addresses for all its
  talkers, a slot per output, as now. It doesn't keep them across runs.
  Listeners take the destination from ACMP and MSRP.
- **Sharing:** the avb-mrp sharing socket,
  `$XDG_RUNTIME_DIR/avb-mrp-<interface>.sock`, is served as
  triib-endpointd serves it now, for any other program that reserves on
  the port.

```toml
# ~/.config/avb-endpointd/enp2s0.toml
avb_lite = "auto"

[[endpoint]]
instance = 112            # 0x70: plaa's entity ID, kept
default = true
model = "avb-endpointd"
name = "linux-desktop"

[[endpoint.stream]]
direction = "out"
name = "Playback"
channels = 2

[[endpoint.stream]]
direction = "in"
name = "Listen"
channels = 2

[[endpoint]]
instance = 0
model = "triib"
name = "Host entity 1"
# streams as triib writes them now
```

### The default entity

One entity on each interface is the default, `default = true`: this
computer on the network for any app (the user, 2026-11-09).

- **Always there:** it runs whenever the daemon runs. The daemon makes
  it from its default values where the file lacks it, as it first
  starts, and again at once where a person removes it by hand.
- **Its default values:** instance 0x70, plaa's entity's before it, so
  the bindings controllers made to plaa's entity by entity ID and index
  carry over, or the lowest free instance where another entity takes
  0x70; named for the computer's host name; avb-endpointd's model; a
  2-channel "Playback" output 0 any app plays into and a 2-channel
  "Listen" input 0 any app records from, each with its virtual device,
  `avb-<entity id>-out-1` and `-in-1`.
- **No app removes it:** `config::edit` keeps it, whatever an app's
  change does to the list, even replacing the whole list, as triib's
  preset restore does. An app or a controller may rename it or change
  its streams' settings. No app makes another entity the default.
- **The others** belong to no app: any app, or a person by hand, adds,
  changes and removes them. The daemon's own write-back merges into
  the current file by instance, under the lock.
- **One entity ID, one entity:** where another program on this computer
  advertises the entity ID of one of the daemon's entities, as an older
  plaa running its own entity at 0x70 does, the daemon holds that
  entity off the network, without a departing message, until the other
  departs or its advertisement runs out. The status marks it held.

### Models

Each entity's identity in AEM comes from a model file an app's
package installs, or avb-endpointd's own for the default entity, `/usr/share/avb-endpointd/models/<model>.toml`:

- the vendor and model names;
- the firmware version, from the app's package version;
- the logo images.

```toml
# /usr/share/avb-endpointd/models/plaa.toml
vendor_name = "Scramble Tools"
model_name = "plaa"
firmware_version = "0.6.0"    # the daemon's when left out
logo = "plaa.png"             # beside the file
vendor_logo = "scramble-vendor.png"
```

The daemon looks in `avb-endpointd/models` under `$XDG_DATA_HOME`, then
each of `$XDG_DATA_DIRS` (`/usr/local/share`, `/usr/share`), so a dev
build's model can sit in `~/.local/share`. Logos are PNG files, square,
128 pixels a side, of at most 64 KiB.

A model file gives no entity model ID. The daemon makes every one as it
makes triib's now, `DEVICE_MODELS` with a 24-bit hash whose bit 16 is
always set. The hash covers the streams, the daemon's own revision, and
the model file's names and logos:

- The model ID changes whenever what controllers cache under it does, as
  AEM asks: an edit to an entity's streams, or another model. Controllers
  cache an entity's descriptors by its model ID, so a pinned ID would go
  stale.
- The firmware version stays out of the hash. Controllers read it again
  at each enumeration, as la_avdecc (and so Hive) keeps it with the
  entity's dynamic state, so a new version shows without a new model ID.
  Hashed, every release, and every rebuild whose version names a commit,
  would make each controller read the whole model again for nothing.
- Two entities with the same streams but different models get different
  IDs, though both use the one family.
- All the daemon's model IDs stay in the one registered family, the
  values of the registry's entity model pool (`8C:1F:64:36:C0:xx:xx:xx`)
  with bit 16 set, which `is_device_model` checks. A base for each
  model would need a new pool, and would leave fewer bits for the hash.

Each identity moves out of the daemon's code and into its app's file:

- triib's: today's model name, LOGO and VENDOR_LOGO;
- plaa's: what plaa's entity shows now ("Scramble Tools", "plaa",
  plaa's and the vendor's logos).

plaa's entity therefore takes a new model ID, in place of
`0x8c1f6436c0200005`. The daemon serves its own descriptor tree for
it, which differs from plaa's own.rs, such as in media clock inputs as
clock sources and dynamic input mappings, so controllers must read it
afresh. That costs a read of the new model once. plaa's entity ID
(instance 0x70) stays, and so does every binding by entity ID and
stream index. Its stream ID follows the daemon's rule. When plaa moves
onto the daemon, it tells the ESP session that the model ID and the
stream ID change, and the registry retires plaa's model block.

A model the daemon can't find, or can't use, falls back to a generic
one, "Scramble Tools" and "avb-endpointd entity" with the vendor logo,
and the status says so.

### Entity IDs

The daemon derives IDs as triib-endpointd does:
`mac[0..3] | 0xff | instance | mac[3..6]`. plaa's entity is instance
0x70 today with the same rule, so it keeps its ID. Controllers' saved
bindings to it, the ESP bench's among them, go on working. Its stream
ID follows the daemon's rule. Listeners and controllers bind by entity ID
and stream index, not by stream ID.

## Audio devices

As triib-pipewire makes them now, moved out of triib as
`avb-pipewire`:

- **Nodes:** an `Audio/Sink` per stream output and an `Audio/Source`
  per stream input. Each is a follower node with F32 samples, AUX
  channel positions and `node.virtual = true`.
  - The description is "<entity name> <stream name>".
  - `node.name` becomes `avb-<entity id>-out-<n>` or `-in-<n>`, from
    today's `triib-` prefix. It is stable from then on, so WirePlumber
    returns an app's stream to its device after the daemon or PipeWire
    restarts.
  - From step 2 it carries `node.network = true`.
- **Lifetime:** a device exists while its entity runs, whether or not a
  listener is connected, so an app can pick it at any time.
  - With no listener connected, what plays into a playback device is
    dropped past four times its margin.
  - With no talker bound, a recording device gives silence and counts
    an underrun.
  - A talker that starts drops what waited.
- **Clock:** the device runs on PipeWire's graph clock. The stream side
  converts to gPTP's clock and the stream's rate, the ratio steered by
  the buffer's fill, as now, with a 10 ms margin.
- **Rate changes, from step 2:**
  - Each device runs at one fixed rate, the graph's `default.clock.rate`
    (normally 48 kHz).
  - When a controller changes a stream's rate, the daemon restarts only
    that talker or listener, whose shared source or sink already
    converts rates. Apps playing into the device stay connected.
  - Before step 2, a rate change dropped and recreated every device.
  - The conversion interpolates cubically with no low-pass filter. That
    is fine for 48 kHz up to 96, but would alias taking a 96 kHz device
    down to a 48 kHz stream. That is one more reason to keep the devices
    at the graph's rate.

## plaa on the daemon

On Linux, plaa is a media app like any other: it has no entity of its
own, and the default entity takes its place and its entity ID.

- **The talker's audio:** the copy of the media window, which plaa now
  pushes into its in-process buffer, plaa plays into the default
  entity's playback device, found by the status's default entity, as a
  second output stream. Other apps can play into the same device, and
  PipeWire mixes them.
- **The input:** plaa records from the default entity's recording
  device into a player window, as `Listened` does now.
- **Status:** plaa reads the status file for the default entity's state,
  gptpd's notes and failures. It shows them where it shows its own now.
- **On and off:** plaa's Network Audio can put this computer on the
  network, starting and enabling the unit, or take it off. Its "Send
  what plays" going off only stops plaa playing into the device, as
  disabling the unit would take triib's entities off the network too.
- plaa's entity name setting (`avb_name`) goes on Linux: the default
  entity is the computer's, renamed by a controller or in the file.

What leaves plaa on Linux, as the in-process entity goes (see
"Decided in review"):

- the entity: own.rs, own/lite.rs, sound.rs and reservation.rs;
- its MRP participants and the sharing client;
- plaa-netraw, which avb-netraw replaces;
- plaa-link@ and the gptpd starting logic.

plaa's controller stays, with its controller ID (0xfc in the instance
octet), for browsing, details and binding listeners to the default
entity, as its Play does now.

On a Mac nothing changes: plaa plays into the Mac's own entity as now.
On Windows there is no entity, as decided.

## triib on the daemon

- host.rs writes the interface's file under the lock, giving its
  entities triib's model, and starts the unit instead of spawning
  triib-endpointd. It reads status and meters at the new paths. triib's
  package installs triib's model file. triib shows the default entity
  with the others, and can't remove it.
- **What goes:**
  - triib's "entities file" setting and move_entities, since the path
    is fixed per interface;
  - triib-endpointd, triib-pipewire, triib-netraw, triib-link@ and
    triib-link-reset@;
  - the spawn: process_group(0), the reaping thread and the log file.
- **What stays:** what triib's window offers for its entities.

## Migration

- **triib's entities:** the first time a person's daemon finds no file
  for an interface, it imports `$XDG_DATA_HOME/triib/entities.toml` for
  that interface. It gives the entities triib's model and pins their
  instances.
- **plaa's entity:** the default entity takes its entity ID, 0x70, so
  controllers' bindings to it carry over; its model ID and stream IDs
  change, and the ESP bench is told first.
- **Device names:** the devices' names change from `triib-` to `avb-`,
  so WirePlumber won't return apps' streams to them the first time. An
  app that played into a triib device picks the new one again once.
- **Old processes:** the daemon binds both claim names while old
  versions can exist (see "One interface, one person"). plaa's entity
  steps aside from MRP on its own MAC as now, so an old plaa and a new
  daemon don't fight.

## The data path

The stream threads stay in the daemon, with no second daemon for them
(avb-streamd is shelved; `streamd.md` says why). Two later steps make
their timing firmer:

- **Locked pages:** the daemon locks (mlock) and touches ahead of time
  only what the stream threads use, their stacks, rings and frame
  buffers, so a page fault never holds one up. That stays under the
  8 MiB a user unit may lock (systemd's DefaultLimitMEMLOCK, 8388608).
  The package sets `vm.compact_unevictable_allowed = 0`, so compaction
  leaves locked pages alone. This waits on the pages being counted.
- **Launch time** (`launch-time.md`), once gptpd's follow-tai keeps the
  PTP hardware clock near CLOCK_TAI: the link units, as root, put mqprio
  and ETF on the interface, and avb-netraw sets SO_TXTIME with
  CLOCK_TAI on a stream's socket before passing it over, as a clock
  other than CLOCK_MONOTONIC takes CAP_NET_ADMIN. Giving avb-netraw
  `cap_net_admin` is the user's decision.

The link units stay as they are.

## Packaging

- **This repository** holds:
  - `crates/avb-endpointd`, the library on crates.io and the daemon
    behind its feature, and `crates/avb-pipewire` (from triib-endpointd
    and triib-pipewire);
  - the `avb-netraw` helper;
  - the user unit, the link units and their polkit rule;
  - Arch, Debian and Fedora packages, released as gptpd is.
- **Dependencies:** the package depends on gptpd, rtkit and PipeWire. It
  sets avb-netraw's capability as it installs.
- **What installing says:**
  - a user service runs this computer's AVB entities when an app turns
    one on, and at each desktop login after;
  - a helper may open raw network sockets;
  - starting the link unit changes the interface's transmit settings,
    EEE and PAUSE, and the link reconnects once if EEE or PAUSE were on.
- **plaa and triib** each install their model files and depend on
  avb-endpointd, as plaa's AVB on Linux already needs gptpd.

## Testing

- **Unit:**
  - apps and the daemon changing one file under the lock;
  - the default entity kept through any edit, and made again from its
    default values where removed by hand;
  - instances pinned on import;
  - a missing model;
  - model IDs: the same streams under two models, and an edit to the
    streams, each give a new ID, and a new firmware version doesn't;
  - importing triib's file;
  - stable node names.
- **One host:**
  - triib's entities beside the default entity on one daemon;
  - an older plaa's entity at 0x70 holding the default off the network;
  - pw-play or mpv plays into plaa's device, and a listener on a veth
    pair receives it;
  - two interfaces with two instances of the template;
  - the daemon is killed: systemd restarts it, the devices come back,
    and WirePlumber returns the app's stream;
  - PipeWire is restarted: the daemon restarts with it;
  - a refusal (another person's claim) doesn't loop;
  - an SSH-only login doesn't start it;
  - the person logs out: entities depart, with ADP's departing and
    MSRP's leave.
- **Bench, with the ESP session:** esp-synth listens to plaa's entity
  through the daemon while mpv plays into its device, for three hours.
  Compare frames late, starves and gaps with plaa's in-process numbers.
- **Old with new:** an old plaa with the daemon, an old triib-endpointd
  (another person's) beside the daemon, and triib before and after its
  move.

## Proposal: network devices as sinks and sources

Not decided: for the user to decide on.
Nothing here changes how triib's features work: triib's entities,
devices and controller go on as they are.

The aim is that a media app needs nothing AVB-specific: plaa, mpv or a
DAW plays into a PipeWire device and records from one, as it would
speakers and a microphone. The default entity (step 2) is this
computer on the network, which controllers route. This adds the other
way round: the network's speakers and amp inputs, each a sink of its
own, which connect when an app plays into them, and remote talkers as
sources. PipeWire already works that way for other networks: AirPlay
receivers, Snapcast and ROC appear as local sinks named for the remote
device.

It has to serve a home with a few AVB speakers as well as a venue's
Milan network (the user, 2026-11-10). At home there is nothing to set
up: a new speaker shows in every app's output list by itself. Choosing
which devices show is for networks large enough to need it.

### Which devices show

Two modes for each interface, in its file:

- **`destinations = "all"`, the default.** Every compatible listener
  input the daemon finds on another device gets a sink of its own, once
  ADP announces its entity and the daemon has read its model, whether
  the device also talks or not, as an interface or an amp with a
  monitor output does.
  - **This computer's own entities are left out:** the daemon's,
    triib's, and any other advertised from this MAC. A sink into one
    would play into a local listener whose recording device another app
    may play back, a loop, and the desktop's own entities would fill
    every sound menu. A person who wants one pins it.
  - An idle sink costs the network nothing: no talker, reservation or
    bandwidth until an app plays into it. Ten speakers are ten PipeWire
    nodes.
  - **Used means remembered.** The first time an app plays into a
    device, the daemon pins it (below). A device never used comes and
    goes with the device; one used keeps its sink while it is off the
    network.
  - **Hidden:** a device on the file's `hide` list gets no sink, such
    as a subwoofer feed or a mixer's aux input. Leaving out a few is
    less work than choosing every one.
- **`destinations = "pinned"`.** Only pinned devices get sinks, for a
  network where every amp and speaker input in every desktop's sound
  menu would make a wrong pick too easy. The status still lists the
  devices found and not pinned, so a front end can say new ones are
  there and offer to pin them.

The daemon never changes the mode itself: switching to pinned as the
list grows would make every sink vanish at once because a speaker was
plugged in. A front end may suggest pinned mode past a dozen or so.

`sources` takes the same two values for remote talkers, as recording
devices, with the same rules, but is `"pinned"` by default. People play
to speakers far more than they record from the network; "all" would
double what every recording picker lists; and a meeting app could take
up a microphone in another room that no one chose. A front end offers
the talkers found, to pin.

### The settings

They sit at the top level of the interface's file, changed under the
lock like any change, by a person, an app, the tool or the daemon:

```toml
# ~/.config/avb-endpointd/enp2s0.toml
destinations = "all"            # or "pinned"
sources = "pinned"              # or "all"

[[hide]]
entity = "0x001b92fffe0123ab"
input = 0
name = "Subwoofer"              # for reading; the ID and index count

[[pin]]
entity = "0x001b92fffe0456cd"
input = 0
name = "Living room"
remembered = true               # pinned by the daemon on first use
```

- Entries are tables rather than `"<id>:<index>"` strings, so each
  carries the device's name beside its ID. `input` is a listener's
  input and `output` a talker's, each the descriptor's index, counted
  from 0 as AEM counts it.
- The daemon writes remembered pins into the file itself, merged by
  entity and index, so a person sees them, and forgets a device by
  removing its entry.
- `name` is only for reading. The daemon fills it in and keeps it
  current; no node name uses it.

### The pools

The default entity carries them after its Playback output 0 and Listen
input 0, with no devices of their own: `destination_talkers` outputs
(4 when left out) and `source_listeners` inputs (4). Their sizes are
settings, not one stream per device, because streams are in the model
ID's hash: a stream added for every device would send every controller
back to read the model each time one comes, goes or is pinned.

### Destination sinks and sources

- **One device for each one shown:** in "all" mode while it is on the
  network, and while it is pinned, whether on the network or not. What
  plays into a pinned device that is off the network is dropped, and
  the status says so, rather than the sink vanishing and WirePlumber
  moving the app to the desktop's speakers without a word.
  - `node.name` is `avb-dest-<entity id>-in-<input>` or
    `avb-src-<entity id>-out-<output>`, from the entity ID and index,
    so WirePlumber's memory of each app's choice survives a rename;
  - one per compatible input or output, not one per device, as a sink
    across several inputs would split channels the pools don't, and an
    amp with several inputs is where a person wants to choose; the
    description is "<device name> <stream name>", or the device's name
    alone where it has one compatible stream that way, as most home
    speakers do;
  - `node.network = true`, and a low session priority, so WirePlumber
    never makes one the default and desktop sounds never reserve on the
    network; the default entity's own devices take the same;
  - at the graph's rate, as every device is, with the stream's side
    converting.
- **Connecting.** When the first app stream links to a destination
  sink, paused or not, the daemon takes a talker from the pool, gives it
  a format the listener takes at its current rate (read over AECP,
  never changing the listener's rate), and sends ACMP's connect to the
  listener as its own controller. A source works the same way the other
  round, binding a pool listener to the remote talker, which takes
  nothing from anyone, as talker streams are multicast.
  - **Busy:** a listener bound to another talker, or acquired or locked
    by another controller (CONTROLLER_NOT_AUTHORIZED), is busy. The
    daemon doesn't take it over and doesn't retry in a loop; the status
    says so. Taking a listener over stays a deliberate act in a
    controller, as plaa's Play is now. This holds in "all" mode too,
    so a listener another controller routes is never taken.
  - **Releasing:** after the last app stream unlinks, and 30 s more, the
    daemon disconnects, so pauses, seeks and track changes don't cost an
    ACMP and MSRP round each. It disconnects only a binding it made that
    no one has changed since.
  - **Running out:** a pool stream per connected device, given back with
    the binding. When the pool, the MAAP block (32 talkers) or the
    bridge's bandwidth runs out, the device's status says so.
- **The daemon's controller** has its own entity ID, `FF-FB` in the
  instance octets, beside triib's controller (`FF-FE`), triib-cli's
  (`FF-FD`) and plaa's (`FF-FC`). It advertises no entity, so nothing new
  shows in triib's or Hive's lists, and it acts only on the devices it
  shows, when an app plays into or records from one.
- **Status:** every device found, pinned or not, in one list with the
  same fields, so one list drives a front end's toggles: entity ID,
  input or output index, name, whether it is pinned, remembered or
  hidden, and its state as a plain value an app shows as it shows a
  talker's: `idle`, `connected`, `busy`, `offline`, `no-talker` or
  `no-bandwidth`.

### The tool and the apps

- **A neutral tool:** `avb-endpointd on|off`, `list`, `pin|unpin`,
  `hide|show` and `mode destinations|sources all|pinned`, with
  `--interface`, taking a device's name or entity ID. It starts or stops the unit and edits the
  file under the lock. No app is needed. Apps do the same through the
  library, `config::edit` and `unit`.
- **plaa's** Network Audio becomes a front end: this computer on or off
  the network, the devices found with hide and pin toggles, new devices
  in pinned mode, the devices' and gptpd's states from the status, and
  its controller for browsing, details and the explicit takeover. Its
  audio plays into a sink, and listens from a source, by node name.
  Whether it also goes on playing on the computer's own speakers, as
  "Send what plays" does now, is plaa's own feature.
- **triib** is unchanged. It may later offer the same as a front end.
- **Without an AVB app,** a person has the tool or the file. A desktop's
  sound settings can't hide a sink for good, so a small settings panel
  of the daemon's own, or a desktop-neutral way in, may follow later.

### Order and decisions

The daemon's controller, the pools and the network devices are a step
of their own, after step 2 and before or after plaa's step, which by
then plays into the default entity's devices.

Settled, with plaa's review (the user, 2026-11-10):

- destinations "all" by default, remembering on first use, with `hide`,
  and "pinned" for large networks;
- this computer's own entities left out of "all";
- one sink per compatible input, not per device;
- devices that also talk shown as well;
- sources "pinned" by default;
- the status listing every device found with the same fields.

Decisions for the user:

- whether to adopt it, and when;
- the 30 s release;
- pools of 4;
- `FF-FB` for the daemon's controller.

Later: group sinks, several listeners on one multicast talker playing
the same audio, as AVB does natively; a settings panel.

## Plan

1. **Move without changing behaviour.** In the avb repository, where it
   began:
   - triib-endpointd and triib-pipewire become avb-endpointd (library
     and daemon) and avb-pipewire;
   - avb-netraw, the user unit template and the link units;
   - the step 1 changes above: the lock per interface, the interface
     from `%i`, no invented first entity, both claim names, restart
     statuses, the link retry, and the three link names.

   triib then moves onto it, in triib's session, when the user asks.
2. **The default entity, models, pinned instances, the file lock and
   inotify.** Also devices at a fixed rate, `node.network`, and
   migration.
3. **plaa onto it.** The copy of what plays goes into the device, and
   the input is recorded from its device. The in-process entity and its
   MRP code come out on Linux. plaa's README and PLAN catch up: the
   PLAN's AVB section is already behind the code.
4. **Packages, releases and the bench run.**

## Decided in review

- One daemon per interface, as a user unit template. This matches the
  claim, the entity IDs taken from the MAC, gptpd@ and the link units.
- The configuration file stays the apps' interface, with the lock,
  pinned instances and inotify. There is no control socket yet: the
  daemon would still keep a file, and the status file reports outcomes.
  Revisit when an app needs an answer at once.
- An app enables the unit when it adds an entity. A package enabling it
  for every person would run AVB for accounts that never asked.
- The configuration lives in `$XDG_CONFIG_HOME/avb-endpointd/`
  (`~/.config`), not triib's data folder, as a file a person may edit
  or back up (the user, 2026-11-08).
- The link units give the first transmit queue fq_codel, not fq (the
  user, 2026-11-09).
- avb-endpointd, avb-pipewire and avb-netraw live in their own
  repository, Linux-AVB-Endpoint, as gptpd and avb-bridged do, with
  avb's protocol and stream crates from crates.io (the user,
  2026-11-09).
- Entities belong to no app: one default entity on each interface,
  which no app removes, and the others any app adds and removes. The
  default exists whenever the daemon runs, and one removed by hand is
  made again from its default values (the user, 2026-11-09).
- avb-streamd is shelved: the streams' data path stays in avb-endpointd,
  with locked pages and launch time as later steps (the user,
  2026-11-08).
- plaa on Linux runs its entity only through avb-endpointd, and its
  packages depend on it (the user, 2026-11-08). The in-process entity,
  its MRP sharing and step-aside code, and plaa-netraw go.
  - This supersedes the 2026-10-21 ruling's preference for shared code
    that each program runs in its own process.
  - Its point stands: plaa needs a neutral avb package, not triib.
