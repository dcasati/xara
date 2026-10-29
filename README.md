# xara

a VARA-compatible modem for linux. tested against VARA HF 4.9.0 and FM 4.4.0.

## minimum config

set your call sign, HF bandwidth or FM mode, audio, and (if using a radio)
radio and ptt. do this with command line flags or a config file.

    xara --call YOURCALL --hf 2300 --audio alsa \
         --capture plughw:CARD=Device,DEV=0 \
         --playback plughw:CARD=Device,DEV=0

to transmit, add `--radio` and `--ptt` flags.

## other features

 - web UI for configuration and management
 - all the usual TCP ports (works with your favorite software)
 - digipeating (enable by call with `--digi N1CALL`)
 - `xara-monitor`: passive VARA HF monitor (with web UI)

## building

install deps:

    apt install libpulse-dev libasound2-dev

then install from here:

    cargo install --locked --git https://github.com/x0rro/xara xara

alternatively for a local setup you can do:

    cargo build --release
    cargo test

## options

    --config FILE           read key = value options from FILE

    --call CALL             your callsign (required)

    --audio pulse|alsa      pulse vs alsa audio path
    --capture SOURCE        audio source
    --playback SINK         audio sink
    --tx-buffer-ms N        TX audio queue, raise on underruns (30-500, default 40)

    --hf 500|2300|2750      HF mode, bandwidth setting
    --fm narrow|wide        FM mode, narrow vs wide

    --radio                 enable radio mode
    --drive-db D            TX drive (<= 0 dB, default -5)
    --ptt [see section]     PTT has a lot of options

    --ui-port N             serve web UI on port N (default off)
    --ui-addr A             webui listen address (default 127.0.0.1)
    --ui-host NAME          accept as HTTP Host for UI (repeatable)
    --ui-token T            UI token (default: random)
    --ui-control            DANGER: remote token-holders can key radio
    --state-dir DIR         web UI settings storage (default: set by systemd)

    --addr A                KISS/command address (default: 127.0.0.1)
    --port N                command port (< 65535, default: 8300)
    --kiss-port N|off       KISS TCP port (default: 8100)
    --persistence P         KISS p-persistence

    --registered            enable registered speeds (must set build flag)

    --digi CALL             FM digipeat for CALL (default: off)
    --record PATH           record rx, tx and PTT to FLAC (or WAV if .wav)
    --log FILE              append UTC-timestamped output to FILE
    --quiet                 no output
    --version and --help    guess

## PTT settings

we support the following PTT methods:

 - rts and dtr on a serial port
 - cm108/cm119 HID GPIO
 - hamlib rigctld CAT
 - vox (not recommended)

you can adjust the following PTT settings as well:

 - PTT lead time
 - PTT tail time
 - max PTT

summary of settings:

    --ptt vox                   use VOX (just don't bother)
    --ptt rts:DEV               key serial port on rts, hold dtr low
    --ptt dtr:DEV               key serial port on dtr, hold rts low
    --ptt cm108:DEV[:GPIO]      cm108/cm119 HID GPIO (default GPIO: 3)
    --ptt rigctld[:HOST:PORT]   hamlib rigctld CAT on HOST:PORT

    --ptt-lead-ms N             PTT held for N ms before xmit (default: 0)
    --ptt-tail-ms N             PTT held for N ms after xmit (default: 100)
    --ptt-max-s N               release PTT after N sec max (1-120, default: 30)

## xara-monitor

`xara-monitor` listens to VARA HF traffic between other stations and never
transmits. it is considerably more demanding than `xara` and probably won't run
on a pi.

    xara-monitor --audio alsa --capture plughw:CARD=Device,DEV=0

options:

    --wav FILE              load from a wav file
    --stdin RATE            s16le mono at RATE Hz

    --audio pulse|alsa      pulse or alsa audio path
    --capture SOURCE        audio source

    --hf 500|2300|2750      HF bandwidth (default 2300)

    --call CALL             known call (repeatable)
    --calls FILE            list of known calls, one per line
    --no-blind              only attempt to resolve known calls

    --ui-port N             read-only web UI on port N (default: off)
    --ui-addr ADDR          web UI listen addr (default: 127.0.0.1)
    --ui-host NAME          accept as HTTP Host for UI (repeatable)

    --key-idle MIN          max target idle time (default: 30, 0 disables)
    --dump                  dump entire payloads
    --verbose               report unconfirmed blind detections
    --json                  print newline-delimited JSON records
    --log FILE              append UTC-timestamped output to FILE
    --version and --help    guess

## registered mode

compile with the `registered` feature for higher speeds if you have a
registered copy of VARA, then run with `xara --registered ...`.

## copyright

copyright (c) 2026 x0rro
released under MIT license, see LICENSE
