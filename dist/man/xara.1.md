---
title: XARA
section: 1
header: User Commands
date: 2026-10-29
---

# NAME

xara - VARA-compatible HF/FM modem

# SYNOPSIS

**xara** [**--audio pulse**|**alsa**] **--call** *CALL* **--capture** *SOURCE* **--playback** *SINK*
[*options*]\
**xara** [**--audio pulse**|**alsa**] **--radio --ptt** *PTT* **--call** *CALL* **--capture** *SOURCE*
**--playback** *SINK* [*options*]

# DESCRIPTION

**xara** is a modem compatible with VARA HF and VARA FM. It supports audio via
ALSA and PipeWire/Pulse. It also exposes TCP ports for VARA's command and data
along with KISS for applications such as Pat. See the README for full details.

# OPTIONS

**--config** *FILE*

:   Read options from *FILE*. Any options given on the command line override
    these. Each line in the file is one option without its leading dashes:
    *key* **=** *value*. For example, **capture = plughw:CARD=Device,DEV=0**.
    Repeated keys are treated the same way as repeated options.

    Without **--config**, xara reads *$XDG_CONFIG_HOME/xara/xara.conf* (by
    default *~/.config/xara/xara.conf*) if it exists.

**--audio** pulse|alsa

:   Audio backend. Defaults to Pulse if compiled in, otherwise ALSA.

**--capture** *SOURCE*

:   Pulse source or ALSA PCM to receive from (required).

**--playback** *SINK*

:   Pulse sink or ALSA PCM to transmit into (required). Without **--radio**, it must be
    a Pulse null sink or the ALSA **null** PCM.

**--tx-buffer-ms** *N*

:   Transmit audio queued ahead of the sound card, in ms. 30-500 (default 40).
    Raise it if xara logs playback underruns: try 80, then 160. xara times its
    transmissions around the extra delay, but keep it as low as runs without
    underruns.

**--call** *CALL*

:   Initial MYCALL (required; N0CALL is refused).

**--hf** 500|2300|2750

:   Run as VARA HF with this bandwidth (default 2300); not with **--fm**.

**--fm** narrow|wide

:   Run as VARA FM instead of HF.

**--digi** *CALL*

:   FM only: also digipeat for *CALL*.

**--record** *PATH*

:   Record RX, TX, and PTT to FLAC (or WAV if filename ends in .wav).

**--port** *N*

:   Command port (default 8300); data is on *N*+1.

**--kiss-port** *N*|off

:   KISS TCP port (default 8100).

**--persistence** *P*

:   KISS p-persistence, 0 to 1 (default 0.3).

**--addr** *A*

:   Listen address (default 127.0.0.1).

**--drive-db** *D*

:   TX drive in dB, at most 0 (default -5).

**--log** *FILE*
:   Append UTC-timestamped output to *FILE*.

**--quiet**

:   No trace output. Lines are still logged when using **--log**.

**--ui-port** *N*

:   Serve the web UI on port *N*: with bonus data at **/stream**. Off by default.

**--ui-addr** *A*

:   Web UI listen address (default 127.0.0.1).

**--ui-host** *NAME*

:   Also accept *NAME* in the HTTP Host header (repeatable).

**--ui-control**

:   **DANGER** allows a remote web user with the UI token to key the radio,
    make config changes, and generally ruin your day. Be careful when allowing.

**--state-dir** *DIR*

:   Where to keep settings saved from the web UI. Defaults to
    **$STATE_DIRECTORY** when launched with systemd. If not included, settings
    are not saved. Web settings override command line parameters, which
    override config file entries.

**--ui-token** *T*

:   UI secret key required to remotely control the rig. If not supplied, xara
    generates a random token and keeps it in *DIR*/ui-token.

**--radio**

:   Transmit over a real radio. Requires **--ptt**.

**--ptt** *PTT*

:   PTT method; requires **--radio**. One of **vox** **rts:***DEV* or
    **dtr:***DEV*, **cm108:***DEV[:GPIO]*, or
    **rigctld**[**:***HOST***:***PORT*]. For full details see README.

**--ptt-lead-ms** *N*

:   Hold PTT for N ms before transmit. 0-1000 (default 0).

**--ptt-tail-ms** *N*

:   Hold PTT for N ms after transmit. 0-1000 (default 100).

**--ptt-max-s** *N*

:   Release PTT after *N* seconds max, 1-120 (default 30).

**--registered**
:   Enable registered speeds (must set build flag)

**-h**, **--help**, **-V**, **--version**

:   Print the usage or the version and exit.

# EXIT STATUS

**0**

:   Normal exit.

**1**

:   PTT error (including failed release), or audio, device or port error.

**2**

:   Bad command line.

**75**

:   A restart asked for from the web UI could not re-execute xara; a service manager
    should start it again.

**101**

:   Stopped after a panic. PTT will have been released.

**130**

:   Stopped by SIGINT, SIGTERM, SIGHUP or SIGQUIT. PTT will have been released

# SAFETY

If you kill with SIGKILL, PTT may not be released!

# FILES

*/etc/xara/STATION.conf*

:   The config file of the **xara@STATION** system service. Must be readable by the
    **xara** user (mode 0640 recommended if it contains **ui-token**).

*~/.config/xara/xara.conf*

:   The config file of the **xara** user service and used when xara is run
    without **--config**.

*/var/lib/xara/STATION/settings*

:   Settings saved from the web UI for **xara@STATION**
    (*~/.local/state/xara/settings* for the user service). One *key* **=**
    *value* per line, and the keys are the options without their leading
    dashes.

*/var/lib/xara/STATION/ui-token*

:   The web UI token if **--ui-token** is not provided on the command line.
    *~/.local/state/xara/ui-token* for the user service.

# ENVIRONMENT

**XARA_UI_TOKEN**

:   The web UI token xara passes to itself when it restarts if **--ui-token**
    is not provided on the command line.

**STATE_DIRECTORY**

:   Used as **--state-dir** if that is not included on command line. systemd
    sets it from **StateDirectory=**.

# SEE ALSO

**xara-monitor**(1), **rigctld**(1), **pactl**(1), **aplay**(1)

README.md in the xara source, https://github.com/x0rro/xara
