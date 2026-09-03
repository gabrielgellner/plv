# Changelog

All notable changes to this project will be documented in this file.

## [0.7.0] - 2026-09-03

### Bug Fixes

- hold a sort instead of redoing it for every page ([`868f70e`](https://gitlab.com/gabrielgellner/plv/-/commit/868f70ebb1a8b9ace9091c57945e17337745e46a))
- refuse an oversized sort rather than attempting one ([`e4614b3`](https://gitlab.com/gabrielgellner/plv/-/commit/e4614b3fc81fed94b78c0e4f54696c8c94b0df5c))
- bound memory by what the machine has, and bound the filter at all ([`2593fa5`](https://gitlab.com/gabrielgellner/plv/-/commit/2593fa590382273e0bc324fe9137b8042d0949ff))
- read scan chunks where they live, not from the top each time ([`dd43afb`](https://gitlab.com/gabrielgellner/plv/-/commit/dd43afb4874e1f95ee5922aca77c1b748d21e710))
- bound a scan chunk by bytes, not by a row count ([`7d7d4bc`](https://gitlab.com/gabrielgellner/plv/-/commit/7d7d4bce6bd18bd3c682ff972cdcf1f8e893230a))

### Features

- a filter and a sort at the same time ([`ed1601a`](https://gitlab.com/gabrielgellner/plv/-/commit/ed1601aa596a76d233410b74965542953667d327))
- index where the rows are, so a page can be found ([`f36b051`](https://gitlab.com/gabrielgellner/plv/-/commit/f36b051ff8e044f4b0ce51e8a70604a88523b7e1))

### Miscellaneous

- polars 0.53 → 0.55.2 ([`eef38e7`](https://gitlab.com/gabrielgellner/plv/-/commit/eef38e7a97d040b974e05ff837af50d2d11674be))

## [0.6.0] - 2026-09-02

### Features

- parse and check the view language ([`cc8ea46`](https://gitlab.com/gabrielgellner/plv/-/commit/cc8ea469ebb836e89dba3ea29f8848abbbc2526a))
- wire :select, :hide, :sort and :reset into the viewer ([`241da1c`](https://gitlab.com/gabrielgellner/plv/-/commit/241da1cc9a9e5fcf94aebce21aff84f95460bbdf))
- a bare verb clears the slot it set ([`5871407`](https://gitlab.com/gabrielgellner/plv/-/commit/5871407bb22bc7be105c1e2227f4b1a3eba354c0))
- :filter, resolved to a set of rows rather than composed ([`352cab9`](https://gitlab.com/gabrielgellner/plv/-/commit/352cab9d563a27ede465689ca07c2ccf96afea6e))
- Tab completion on the `:` line, with a candidate panel ([`e0f31e6`](https://gitlab.com/gabrielgellner/plv/-/commit/e0f31e62c92d9b631771ad0ef44318df60c16979))

### Miscellaneous

- keep commit subjects as written in the changelog ([`a70a1e7`](https://gitlab.com/gabrielgellner/plv/-/commit/a70a1e7bdd302b8a4942e711a08ab6986adc6b52))
- drop the version heading from release notes ([`d4ba738`](https://gitlab.com/gabrielgellner/plv/-/commit/d4ba7385bb33f61dbb04341ae74cc2c922a10f53))

## [0.5.0] - 2026-09-01

### Bug Fixes

- stop rendering text cells in quotes ([`8b4c3d6`](https://gitlab.com/gabrielgellner/plv/-/commit/8b4c3d6cbba0956462cd2417f3a83593a90eefcc))
- show an empty cell as a marker rather than the word "null" ([`ecce3d0`](https://gitlab.com/gabrielgellner/plv/-/commit/ecce3d09d2c8acebd6892011c52ddcea21a7483e))
- fit and finish for horizontal layout and movement ([`4ab5cd0`](https://gitlab.com/gabrielgellner/plv/-/commit/4ab5cd00b2bd2aa71d9763d31f88c640aa35729f))
- gg for the first row, and vim's half-screen scroll ([`4fec0ab`](https://gitlab.com/gabrielgellner/plv/-/commit/4fec0ab82c4d156bf9b31bec82269093861e32a7))

### Documentation

- describe the edit buffer in CLAUDE.md ([`f0882ac`](https://gitlab.com/gabrielgellner/plv/-/commit/f0882ac77c647e36c0c82b20e975e824fd0fc733))
- note how cells and empty fields are rendered ([`fff8e42`](https://gitlab.com/gabrielgellner/plv/-/commit/fff8e42ec293f833196641b698ed3b567c1c91e4))

### Features

- read TSV, .tab and .txt files ([`2a3328c`](https://gitlab.com/gabrielgellner/plv/-/commit/2a3328ce2164c5226f39b8c4334bc0107892e8a2))
- edit delimited files in a buffer, written with :w ([`c562559`](https://gitlab.com/gabrielgellner/plv/-/commit/c56255976772d329ff0dbee2af706bfe8cd3fbcb))
- count row numbers from the cursor ([`0061fe4`](https://gitlab.com/gabrielgellner/plv/-/commit/0061fe4c350eae9c73f5b2a73c9620ee8d40c2f3))
- count h and l, like j and k ([`d88c384`](https://gitlab.com/gabrielgellner/plv/-/commit/d88c3845bfcd79fe28c167ed724dfdd09e9c439b))

### Styling

- format the repo with cargo fmt ([`a738036`](https://gitlab.com/gabrielgellner/plv/-/commit/a73803646d94afb9a808565ec1131fbf6d8a8623))

## [0.4.1] - 2026-08-27

### Bug Fixes

- add --version flag ([`458e329`](https://gitlab.com/gabrielgellner/plv/-/commit/458e329f53c86bc9c9de3123cd2a540b2b5d6093))

## [0.4.0] - 2026-08-27

### Documentation

- README section and in-app `?` key overlay ([`1b9fc95`](https://gitlab.com/gabrielgellner/plv/-/commit/1b9fc95dbd29610cd1b9ea9748bd335f9ecfa63c))

### Features

- catalog reader and file pane ([`2c6dbe2`](https://gitlab.com/gabrielgellner/plv/-/commit/2c6dbe2bfd833f84f4e1970dd2d33ed69cb68857))
- snapshot picker with time travel ([`f0595bd`](https://gitlab.com/gabrielgellner/plv/-/commit/f0595bd1c72c1b9e00233c183f664c3e659982ac))

### Miscellaneous

- skip merge commits in the changelog ([`150e6ee`](https://gitlab.com/gabrielgellner/plv/-/commit/150e6eecbed0ee6f36091e2787da8c35e451f9c0))

### Refactoring

- read lakes through the ducklake extension ([`c5dece3`](https://gitlab.com/gabrielgellner/plv/-/commit/c5dece3a252f031af117e5ebd60accac772a7b78))

## [0.3.0] - 2026-03-24

### Features

- multi-column sort with async animation ([`41b2688`](https://gitlab.com/gabrielgellner/plv/-/commit/41b2688cfac764d13f7c9327cb97c2110a4516c2))

## [0.2.0] - 2026-03-24

### Features

- vim-style regex search with async background scanning ([`644d7e2`](https://gitlab.com/gabrielgellner/plv/-/commit/644d7e2c8d7816ec8a689389aedfd3d195f9d311))
- column/cell selection modes with scoped search ([`034ff2a`](https://gitlab.com/gabrielgellner/plv/-/commit/034ff2affeccbec869fad8165de247474174ef7f))

### Miscellaneous

- replace sed with cargo set-version for version bumping ([`8c1c235`](https://gitlab.com/gabrielgellner/plv/-/commit/8c1c235f8dbc897e142b9bf3b5d7bd16fa5937f7))

## [0.1.0] - 2026-03-24

### Documentation

- add README ([`4ab472c`](https://gitlab.com/gabrielgellner/plv/-/commit/4ab472c05bedf21c81516fd32906ffecc74b8fc1))
- add CONTRIBUTING guide covering dev setup and release workflow ([`28d7f22`](https://gitlab.com/gabrielgellner/plv/-/commit/28d7f224fd50cdac324a523d04beb98b2a5d8a29))

### Features

- initial 3-layer CSV/Parquet TUI viewer ([`3fa6ba8`](https://gitlab.com/gabrielgellner/plv/-/commit/3fa6ba87bf25211c6eca99525b87c279d54cae4d))
- cursor-based navigation with row highlight and nG jump ([`23ab64c`](https://gitlab.com/gabrielgellner/plv/-/commit/23ab64c6ec9342bf14ac726f4ef255f49dd13bd2))
- csvlens-style column sizing, truncation, and plain row style ([`2e86d3d`](https://gitlab.com/gabrielgellner/plv/-/commit/2e86d3ddf9775d119af701567d5022ed5d5c0747))
- theme system, separator lines, dynamic row width, partial columns ([`b68896d`](https://gitlab.com/gabrielgellner/plv/-/commit/b68896df26d15267f096985ae9d4690b424e8616))

### Miscellaneous

- add MIT license ([`690d4cd`](https://gitlab.com/gabrielgellner/plv/-/commit/690d4cddf1a58ad1b82cad903913c6438e577cc9))
- ad-hoc codesign on macOS to suppress Gatekeeper popup ([`a045e54`](https://gitlab.com/gabrielgellner/plv/-/commit/a045e54edaf1b250fc1d57c0b66927cec5d0ce6d))
- add git-cliff changelog config and justfile release workflow ([`c88b048`](https://gitlab.com/gabrielgellner/plv/-/commit/c88b048dbfe8b6cc2328016245a5a21531de046f))

### Styling

- fix clippy warnings ([`5cb0dba`](https://gitlab.com/gabrielgellner/plv/-/commit/5cb0dba5f5d4bc0be4d3d3b09fd1e7cd00ff7722))

[0.7.0]: https://gitlab.com/gabrielgellner/plv/-/compare/v0.6.0...v0.7.0

[0.6.0]: https://gitlab.com/gabrielgellner/plv/-/compare/v0.5.0...v0.6.0

[0.5.0]: https://gitlab.com/gabrielgellner/plv/-/compare/v0.4.1...v0.5.0

[0.4.1]: https://gitlab.com/gabrielgellner/plv/-/compare/v0.4.0...v0.4.1

[0.4.0]: https://gitlab.com/gabrielgellner/plv/-/compare/v0.3.0...v0.4.0

[0.3.0]: https://gitlab.com/gabrielgellner/plv/-/compare/v0.2.0...v0.3.0

[0.2.0]: https://gitlab.com/gabrielgellner/plv/-/compare/v0.1.0...v0.2.0

[0.1.0]: https://gitlab.com/gabrielgellner/plv/-/tags/v0.1.0


