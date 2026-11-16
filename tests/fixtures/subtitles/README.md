# Subtitle fixtures

What `src/subtitles.rs` and `src/subtitle_tests.rs` read. Real files are cut to what the tests
need; `.FABRICATED` ones were written by the tool named, around made-up or excerpted text.

| File | Where it came from |
|---|---|
| `subdl-578492-1497278.excerpt.zip` | SubDL's English subtitle for His Girl Friday (1940, public domain), `https://dl.subdl.com/subtitle/578492-1497278.zip`, fetched 2026-11-16. Its text cut to the first 20 of its 1,878 cues (UTF-8 BOM and CRLF kept) and packed again with Python's `zipfile` under the original's entry name, date, deflate method and archive comment (`Subscene`) |
| `subdl-578514-530576.excerpt.srt` | SubDL's Romanian subtitle for the same film, `https://dl.subdl.com/subtitle/578514-530576.zip`, fetched 2026-11-16: the first 8 cues of its file as served, bytes unchanged (windows-1250), without cue 2, the translator's credit |
| `his-girl-friday.excerpt.FABRICATED.srt.gz` | The first 6 cues of the English file, gzipped with Python's `gzip` |
| `season-pack.FABRICATED.zip` | Python's `zipfile` writing to a stream that can't seek, so each entry's sizes follow its data (flag 3) as streamed ZIPs have them: three episodes, a forced file and a readme |
| `traversal.FABRICATED.zip` | Python's `zipfile`: an entry named `../../evil.srt` |
| `bomb.FABRICATED.zip` | Python's `zipfile`, deflate level 9: 17 MB of one cue repeated, 51 KB packed |
| `nested.FABRICATED.zip` | Python's `zipfile`: a ZIP holding a ZIP and a readme |
| `encrypted.FABRICATED.zip` | Info-ZIP 3.0's `zip -P`: one encrypted entry |
| `macos-ditto.FABRICATED.zip` | macOS's `ditto -c -k --sequesterRsrc` of the English excerpt with a quarantine attribute set: the `__MACOSX/._<name>` AppleDouble entry a Mac adds |
