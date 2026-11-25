# usability journeys - 2026-11-25

World: real Navidrome + silent hypodj in a rootless user+net namespace (see usability/).

| journey | state | check | result | notes |
|---|---|---|---|---|
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
| drop-mid-song | blocked | Up Next says the mirrored song is on disk (no stale 'stream only') | FAIL | 129x40: no match for /Thin Air[^\n]*\n[^\n]*on disk/ on screen; 80x24: no match for /Thin Air[^\n]*\n[^\n]*on disk/ on screen |
| drop-mid-song | blocked | half a minute after an unfetchable song is due, the daemon no longer claims it plays | FAIL | truth 'status.elapsed != 0.000' failed (status.elapsed='0.000') |
| drop-mid-song | blocked | half a minute after an unfetchable song is due, the screen says why it is not playing | n/a | n/a: precondition false (status.state='play') |
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
| drop-mid-song | no_network | Up Next says the mirrored song is on disk (no stale 'stream only') | FAIL | 129x40: no match for /Thin Air[^\n]*\n[^\n]*on disk/ on screen; 80x24: no match for /Thin Air[^\n]*\n[^\n]*on disk/ on screen |
| drop-mid-song | no_network | half a minute after an unfetchable song is due, the daemon no longer claims it plays | n/a | n/a: precondition false (status.state='stop') |
| drop-mid-song | no_network | half a minute after an unfetchable song is due, the screen says why it is not playing | FAIL | 129x40: no match for /could(n't/ not) (play/fetch/stream)[^\n]*Ridge Line/Ridge Line[^\n]*(not on disk/unreachable/could(n't/ not))/ on screen; 80x24: no match for /could(n't/ not) (play/fetch/stream)[^\n]*Ridge Line/Ridge Line[^\n]*(not on disk/unreachable/could(n't/ not))/ on screen |
| drop-mid-song | no_network | transport word matches the daemon ninety seconds after due | PASS |  |
| drop-mid-song | no_network | steps 4 within budget 4 | PASS |  |
| drop-mid-song | no_network | every step settled (sign of life within 12s) | PASS |  |
| server-returns | blocked | setup: the Albums pane is the on-disk fallback while the server is gone | PASS |  |
| server-returns | blocked | after the server returns, the Albums pane lists the server's albums again unasked | FAIL | 129x40: none of ['Stream Summit'] on screen; 129x40: forbidden 'showing 1 album on disk' on screen; 80x24: none of ['Stream Summit'] on screen; 80x24: forbidden 'showing 1 album on disk' on screen |
| server-returns | blocked | steps 1 within budget 1 | PASS |  |
| server-returns | blocked | every step settled (sign of life within 12s) | PASS |  |
| server-returns | no_network | setup: the Albums pane is the on-disk fallback while the server is gone | PASS |  |
| server-returns | no_network | after the server returns, the Albums pane lists the server's albums again unasked | FAIL | 129x40: none of ['Stream Summit'] on screen; 129x40: forbidden 'showing 1 album on disk' on screen; 80x24: none of ['Stream Summit'] on screen; 80x24: forbidden 'showing 1 album on disk' on screen |
| server-returns | no_network | steps 1 within budget 1 | PASS |  |
| server-returns | no_network | every step settled (sign of life within 12s) | PASS |  |
