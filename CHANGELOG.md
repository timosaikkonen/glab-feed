# Changelog

## 0.2.3

### Added

- Collect notifications from every accessible project
- Add a --debug log of GitLab requests

### Fixed

- Stop loading commits and unchanged notifications on every poll

## 0.2.2

### Added

- Add a letter hotkey for each copy-menu choice
- Prefix copied link text with the repo name

### Fixed

- Keep the selected merge request when the list changes

## 0.2.1

### Added

- Scroll the tab bar to keep the selected tab visible

### Documentation

- Recapture the README screenshot in color

## 0.2.0

First public release.

- Live-updating terminal list of open GitLab merge requests, one tab per repo, with status, CI, comments, and age
- Notifications feed for comments, replies to your review comments, reviews, review requests, and failed pipelines
- cmux alerts for new activity and approvals, and opening a merge request in a cmux split
- Filters for your own merge requests, selected authors, and title or IID
- Copy the merge request URL, ID, link, or branch
- MIT license
