# usability journeys - 2026-11-25-w18-fixes

World: real Navidrome + silent hypodj in a rootless user+net namespace (see usability/).

| journey | state | check | result | notes |
|---|---|---|---|---|
| connected | online | screen says the server is reachable | PASS |  |
| connected | online | daemon itself answers (the UI talks to a live daemon) | PASS |  |
| connected | online | steps 0 within budget 1 | PASS |  |
| connected | online | every step settled (sign of life within 12s) | PASS |  |
| connected | blocked | screen says the server is NOT reachable | PASS |  |
| connected | blocked | daemon itself answers (the UI talks to a live daemon) | PASS |  |
| connected | blocked | steps 0 within budget 1 | PASS |  |
| connected | blocked | every step settled (sign of life within 12s) | PASS |  |
| connected | no_network | screen says the server is NOT reachable | PASS |  |
| connected | no_network | daemon itself answers (the UI talks to a live daemon) | PASS |  |
| connected | no_network | steps 0 within budget 1 | PASS |  |
| connected | no_network | every step settled (sign of life within 12s) | PASS |  |
| offline-album | online | album list states availability in words (on disk / stream only) | PASS |  |
| offline-album | online | the held album is playing (daemon truth) | PASS |  |
| offline-album | online | the screen names what is playing | PASS |  |
| offline-album | online | steps 3 within budget 6 | PASS |  |
| offline-album | online | every step settled (sign of life within 12s) | PASS |  |
| offline-album | blocked | album list states availability in words (on disk / stream only) | PASS |  |
| offline-album | blocked | the held album is playing (daemon truth) | PASS |  |
| offline-album | blocked | the screen names what is playing | PASS |  |
| offline-album | blocked | steps 3 within budget 6 | PASS |  |
| offline-album | blocked | every step settled (sign of life within 12s) | PASS |  |
| offline-album | no_network | album list states availability in words (on disk / stream only) | PASS |  |
| offline-album | no_network | the held album is playing (daemon truth) | PASS |  |
| offline-album | no_network | the screen names what is playing | PASS |  |
| offline-album | no_network | steps 3 within budget 6 | PASS |  |
| offline-album | no_network | every step settled (sign of life within 12s) | PASS |  |
| drop-mid-song | online | setup: a streamed (not held) song was playing before the drop | PASS |  |
| drop-mid-song | online | control: no false alarm while online | PASS |  |
| drop-mid-song | online | screen transport word matches the daemon (no stale 'playing') | PASS |  |
| drop-mid-song | online | when a song that was never fetched is due: transport word matches the daemon | PASS |  |
| drop-mid-song | online | transport word matches the daemon ninety seconds after due | PASS |  |
| drop-mid-song | online | steps 4 within budget 4 | PASS |  |
| drop-mid-song | online | every step settled (sign of life within 12s) | PASS |  |
| drop-mid-song | blocked | setup: a streamed (not held) song was playing before the drop | PASS |  |
| drop-mid-song | blocked | within 10s the screen says the server is gone | PASS |  |
| drop-mid-song | blocked | screen transport word matches the daemon (no stale 'playing') | PASS |  |
| drop-mid-song | blocked | the next song states availability in words while the server is gone (decision b) | PASS |  |
| drop-mid-song | blocked | when a song that was never fetched is due: still playing, or the screen says why not | n/a | n/a: precondition false (status.state='play') |
| drop-mid-song | blocked | when a song that was never fetched is due: transport word matches the daemon | PASS |  |
| drop-mid-song | blocked | a song mirrored before the drop is still on disk per the daemon (never evicted offline) | PASS |  |
| drop-mid-song | blocked | the deck says the mirrored next song is on disk (no stale 'stream only') | PASS |  |
| drop-mid-song | blocked | half a minute after an unfetchable song is due, the daemon no longer claims it plays | n/a | n/a: precondition false (status.state='stop') |
| drop-mid-song | blocked | half a minute after an unfetchable song is due, the screen says why it is not playing | PASS |  |
| drop-mid-song | blocked | transport word matches the daemon ninety seconds after due | PASS |  |
| drop-mid-song | blocked | steps 4 within budget 4 | PASS |  |
| drop-mid-song | blocked | every step settled (sign of life within 12s) | PASS |  |
| drop-mid-song | no_network | setup: a streamed (not held) song was playing before the drop | PASS |  |
| drop-mid-song | no_network | within 10s the screen says the server is gone | PASS |  |
| drop-mid-song | no_network | screen transport word matches the daemon (no stale 'playing') | PASS |  |
| drop-mid-song | no_network | the next song states availability in words while the server is gone (decision b) | PASS |  |
| drop-mid-song | no_network | when a song that was never fetched is due: still playing, or the screen says why not | PASS |  |
| drop-mid-song | no_network | when a song that was never fetched is due: transport word matches the daemon | PASS |  |
| drop-mid-song | no_network | a song mirrored before the drop is still on disk per the daemon (never evicted offline) | PASS |  |
| drop-mid-song | no_network | the deck says the mirrored next song is on disk (no stale 'stream only') | PASS |  |
| drop-mid-song | no_network | half a minute after an unfetchable song is due, the daemon no longer claims it plays | n/a | n/a: precondition false (status.state='stop') |
| drop-mid-song | no_network | half a minute after an unfetchable song is due, the screen says why it is not playing | PASS |  |
| drop-mid-song | no_network | transport word matches the daemon ninety seconds after due | PASS |  |
| drop-mid-song | no_network | steps 4 within budget 4 | PASS |  |
| drop-mid-song | no_network | every step settled (sign of life within 12s) | PASS |  |
| server-returns | blocked | setup: the Albums pane is the on-disk fallback while the server is gone | PASS |  |
| server-returns | blocked | after the server returns, the Albums pane lists the server's albums again unasked | PASS |  |
| server-returns | blocked | steps 1 within budget 1 | PASS |  |
| server-returns | blocked | every step settled (sign of life within 12s) | PASS |  |
| server-returns | no_network | setup: the Albums pane is the on-disk fallback while the server is gone | PASS |  |
| server-returns | no_network | after the server returns, the Albums pane lists the server's albums again unasked | PASS |  |
| server-returns | no_network | steps 1 within budget 1 | PASS |  |
| server-returns | no_network | every step settled (sign of life within 12s) | PASS |  |
| cover-truth | blocked | setup: the covered album plays and its cover is drawn in cells | PASS |  |
| cover-truth | blocked | setup: the skip landed in the next album and it plays (from disk) | PASS |  |
| cover-truth | blocked | after the skip the screen names the new song | PASS |  |
| cover-truth | blocked | no cell of the previous album's cover remains under the new title | PASS |  |
| cover-truth | blocked | steps 8 within budget 8 | PASS |  |
| cover-truth | blocked | every step settled (sign of life within 12s) | PASS |  |
| info-keeps-deck | online | the info card is open | PASS |  |
| info-keeps-deck | online | the playing title and the transport word stay on the top rows with Info open | PASS |  |
| info-keeps-deck | online | the playing title heads the wide rail with Info open | PASS |  |
| info-keeps-deck | online | the transport word on screen matches the daemon with Info open, at every size | PASS |  |
| info-keeps-deck | online | steps 2 within budget 4 | PASS |  |
| info-keeps-deck | online | every step settled (sign of life within 12s) | PASS |  |
| glance | online | at 80x24 the current song, the next song, the server word and the timer are on one frame | PASS |  |
| glance | online | online: the glance says server online | PASS |  |
| glance | online | at 80x24 the Albums view shows at least 16 album rows | PASS |  |
| glance | online | the view line is whole words or whole letters and ends in the server word, never clipped | PASS |  |
| glance | online | steps 3 within budget 3 | PASS |  |
| glance | online | every step settled (sign of life within 12s) | PASS |  |
| glance | blocked | at 80x24 the current song, the next song, the server word and the timer are on one frame | PASS |  |
| glance | blocked | blocked: the glance says server unreachable | PASS |  |
| glance | blocked | the view line is whole words or whole letters and ends in the server word, never clipped | PASS |  |
| glance | blocked | steps 3 within budget 3 | PASS |  |
| glance | blocked | every step settled (sign of life within 12s) | PASS |  |
| glance | no_network | at 80x24 the current song, the next song, the server word and the timer are on one frame | PASS |  |
| glance | no_network | no_network: the glance says no network or server unreachable | PASS |  |
| glance | no_network | the view line is whole words or whole letters and ends in the server word, never clipped | PASS |  |
| glance | no_network | steps 3 within budget 3 | PASS |  |
| glance | no_network | every step settled (sign of life within 12s) | PASS |  |
| nudge | online | one key skipped to the next song | PASS |  |
| nudge | online | one sentence queued more like this (the queue grew past the album) | PASS |  |
| nudge | online | one sentence armed stop after this album, and the deck says it is held | PASS |  |
| nudge | online | steps 5 within budget 5 | PASS |  |
| nudge | online | every step settled (sign of life within 12s) | PASS |  |
