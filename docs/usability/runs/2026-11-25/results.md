# usability journeys - 2026-11-25

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
| drop-mid-song | blocked | Up Next states availability in words while the server is gone (decision b) | PASS |  |
| drop-mid-song | blocked | when a song that was never fetched is due: still playing, or the screen says why not | n/a | n/a: precondition false (status.state='play') |
| drop-mid-song | blocked | when a song that was never fetched is due: transport word matches the daemon | PASS |  |
| drop-mid-song | blocked | a song mirrored before the drop is still on disk per the daemon (never evicted offline) | PASS |  |
| drop-mid-song | blocked | Up Next says the mirrored song is on disk (no stale 'stream only') | PASS |  |
| drop-mid-song | blocked | half a minute after an unfetchable song is due, the daemon no longer claims it plays | n/a | n/a: precondition false (status.state='stop') |
| drop-mid-song | blocked | half a minute after an unfetchable song is due, the screen says why it is not playing | PASS |  |
| drop-mid-song | blocked | transport word matches the daemon ninety seconds after due | PASS |  |
| drop-mid-song | blocked | steps 4 within budget 4 | PASS |  |
| drop-mid-song | blocked | every step settled (sign of life within 12s) | PASS |  |
| drop-mid-song | no_network | setup: a streamed (not held) song was playing before the drop | PASS |  |
| drop-mid-song | no_network | within 10s the screen says the server is gone | PASS |  |
| drop-mid-song | no_network | screen transport word matches the daemon (no stale 'playing') | PASS |  |
| drop-mid-song | no_network | Up Next states availability in words while the server is gone (decision b) | PASS |  |
| drop-mid-song | no_network | when a song that was never fetched is due: still playing, or the screen says why not | PASS |  |
| drop-mid-song | no_network | when a song that was never fetched is due: transport word matches the daemon | PASS |  |
| drop-mid-song | no_network | a song mirrored before the drop is still on disk per the daemon (never evicted offline) | PASS |  |
| drop-mid-song | no_network | Up Next says the mirrored song is on disk (no stale 'stream only') | PASS |  |
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
