# usability journeys - 2026-11-24

World: real Navidrome + silent hypodj in a rootless user+net namespace (see usability/).

| journey | state | check | result | notes |
|---|---|---|---|---|
| connected | online | screen says the server is reachable | FAIL | 129x40: none of ['connected', 'online', 'server ok', 'reachable'] on screen; 80x24: none of ['connected', 'online', 'server ok', 'reachable'] on screen |
| connected | online | daemon itself answers (the UI talks to a live daemon) | PASS |  |
| connected | online | steps 0 within budget 1 | PASS |  |
| connected | online | every step settled (sign of life within 12s) | PASS |  |
| connected | blocked | screen says the server is NOT reachable | FAIL | 129x40: none of ['unreachable', 'no network', 'server down', 'cannot reach', 'not reachable'] on screen; 80x24: none of ['unreachable', 'no network', 'server down', 'cannot reach', 'not reachable'] on screen |
| connected | blocked | daemon itself answers (the UI talks to a live daemon) | PASS |  |
| connected | blocked | steps 0 within budget 1 | PASS |  |
| connected | blocked | every step settled (sign of life within 12s) | PASS |  |
| connected | no_network | screen says the server is NOT reachable | FAIL | 129x40: none of ['unreachable', 'no network', 'server down', 'cannot reach', 'not reachable'] on screen; 80x24: none of ['unreachable', 'no network', 'server down', 'cannot reach', 'not reachable'] on screen |
| connected | no_network | daemon itself answers (the UI talks to a live daemon) | PASS |  |
| connected | no_network | steps 0 within budget 1 | PASS |  |
| connected | no_network | every step settled (sign of life within 12s) | PASS |  |
| offline-album | online | album list states availability in words (on disk / stream only) | FAIL | 129x40: none of ['on disk', 'downloaded', '(held)', 'stream only', 'not on disk'] on screen; 80x24: none of ['on disk', 'downloaded', '(held)', 'stream only', 'not on disk'] on screen |
| offline-album | online | the held album is playing (daemon truth) | PASS |  |
| offline-album | online | the screen names what is playing | PASS |  |
| offline-album | online | steps 3 within budget 6 | PASS |  |
| offline-album | online | every step settled (sign of life within 12s) | PASS |  |
| offline-album | blocked | album list states availability in words (on disk / stream only) | FAIL | 129x40: none of ['on disk', 'downloaded', '(held)', 'stream only', 'not on disk'] on screen; 80x24: none of ['on disk', 'downloaded', '(held)', 'stream only', 'not on disk'] on screen |
| offline-album | blocked | the held album is playing (daemon truth) | FAIL | truth 'status.state == play' failed (status.state='stop'); truth 'currentsong.Album == Held Harbour' failed (currentsong.Album=None) |
| offline-album | blocked | the screen names what is playing | FAIL | 129x40: none of ['Held Harbour', 'Low Tide'] on screen; 80x24: none of ['Held Harbour', 'Low Tide'] on screen |
| offline-album | blocked | steps 3 within budget 6 | PASS |  |
| offline-album | blocked | every step settled (sign of life within 12s) | PASS |  |
| offline-album | no_network | album list states availability in words (on disk / stream only) | FAIL | 129x40: none of ['on disk', 'downloaded', '(held)', 'stream only', 'not on disk'] on screen; 80x24: none of ['on disk', 'downloaded', '(held)', 'stream only', 'not on disk'] on screen |
| offline-album | no_network | the held album is playing (daemon truth) | FAIL | truth 'status.state == play' failed (status.state='stop'); truth 'currentsong.Album == Held Harbour' failed (currentsong.Album=None) |
| offline-album | no_network | the screen names what is playing | FAIL | 129x40: none of ['Held Harbour', 'Low Tide'] on screen; 80x24: none of ['Held Harbour', 'Low Tide'] on screen |
| offline-album | no_network | steps 3 within budget 6 | PASS |  |
| offline-album | no_network | every step settled (sign of life within 12s) | PASS |  |
| drop-mid-song | online | setup: a streamed (not held) song was playing before the drop | PASS |  |
| drop-mid-song | online | control: no false alarm while online | PASS |  |
| drop-mid-song | online | screen transport word matches the daemon (no stale 'playing') | PASS |  |
| drop-mid-song | online | when a song that was never fetched is due: transport word matches the daemon | PASS |  |
| drop-mid-song | online | steps 4 within budget 4 | PASS |  |
| drop-mid-song | online | every step settled (sign of life within 12s) | PASS |  |
| drop-mid-song | blocked | setup: a streamed (not held) song was playing before the drop | PASS |  |
| drop-mid-song | blocked | within 10s the screen says the server is gone | FAIL | 129x40: none of ['unreachable', 'no network', 'server down', 'connection lost', 'lost the server', 'cannot reach'] on screen; 80x24: none of ['unreachable', 'no network', 'server down', 'connection lost', 'lost the server', 'cannot reach'] on screen |
| drop-mid-song | blocked | screen transport word matches the daemon (no stale 'playing') | PASS |  |
| drop-mid-song | blocked | Up Next states availability in words while the server is gone (decision b) | FAIL | 129x40: none of ['on disk', 'stream only', 'not on disk', 'downloaded'] on screen; 80x24: none of ['on disk', 'stream only', 'not on disk', 'downloaded'] on screen |
| drop-mid-song | blocked | when a song that was never fetched is due: still playing, or the screen says why not | n/a | n/a: precondition false (status.state='play') |
| drop-mid-song | blocked | when a song that was never fetched is due: transport word matches the daemon | PASS |  |
| drop-mid-song | blocked | steps 4 within budget 4 | PASS |  |
| drop-mid-song | blocked | every step settled (sign of life within 12s) | PASS |  |
| drop-mid-song | no_network | setup: a streamed (not held) song was playing before the drop | PASS |  |
| drop-mid-song | no_network | within 10s the screen says the server is gone | FAIL | 129x40: none of ['unreachable', 'no network', 'server down', 'connection lost', 'lost the server', 'cannot reach'] on screen; 80x24: none of ['unreachable', 'no network', 'server down', 'connection lost', 'lost the server', 'cannot reach'] on screen |
| drop-mid-song | no_network | screen transport word matches the daemon (no stale 'playing') | PASS |  |
| drop-mid-song | no_network | Up Next states availability in words while the server is gone (decision b) | FAIL | 129x40: none of ['on disk', 'stream only', 'not on disk', 'downloaded'] on screen; 80x24: none of ['on disk', 'stream only', 'not on disk', 'downloaded'] on screen |
| drop-mid-song | no_network | when a song that was never fetched is due: still playing, or the screen says why not | n/a | n/a: precondition false (status.state='play') |
| drop-mid-song | no_network | when a song that was never fetched is due: transport word matches the daemon | PASS |  |
| drop-mid-song | no_network | steps 4 within budget 4 | PASS |  |
| drop-mid-song | no_network | every step settled (sign of life within 12s) | PASS |  |
