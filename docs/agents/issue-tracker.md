# Issue tracker: GitHub (bp-bitplane org)

Issues live as GitHub issues across the `bp-bitplane` repos, all collected on the org Project **bitplane** (`gh project … 1 --owner bp-bitplane`). Use the `gh` CLI.

## Where an issue goes

- Work in one repo → that repo's issues.
- Work spanning repos, wayfinder maps and stack-wide decisions → `bp-bitplane/bitplane-protocol`.
- Always pass `-R bp-bitplane/<repo>`: a plane holds several repos, so the working directory doesn't decide.
- Add every new issue to the Project: `gh project item-add 1 --owner bp-bitplane --url <issue-url>`.
- Refer across repos as `bp-bitplane/<repo>#<n>`.

## Conventions

- **Create**: `gh issue create -R bp-bitplane/<repo> --title "..." --body "..."` (heredoc for multi-line), then add it to the Project.
- **Read**: `gh issue view <n> -R bp-bitplane/<repo> --json number,title,body,labels,comments`.
- **List**: `gh issue list -R bp-bitplane/<repo> --state open --json number,title,labels`; across all repos: `gh project item-list 1 --owner bp-bitplane --format json`.
- **Sub-issue** (works across repos): `gh api --method POST repos/bp-bitplane/<parent-repo>/issues/<parent>/sub_issues -F sub_issue_id=<child-db-id>`.
- **Database id**: `gh api repos/bp-bitplane/<repo>/issues/<n> --jq .id` (not `#number` or `node_id`).
- **Comment / label / close**: `gh issue comment`, `gh issue edit --add-label|--remove-label`, `gh issue close --comment`.

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external PRs as feature requests; `/triage` reads this flag.)_

## When a skill says "publish to the issue tracker"

Create a GitHub issue in the repo chosen by **Where an issue goes**, and add it to the Project.

## When a skill says "fetch the relevant ticket"

Read it as in **Read** above. Bare `#n` means the current repo.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single issue with **child** issues as tickets.

- **Map**: an issue in `bitplane-protocol` labelled `wayfinder:map`, holding Notes / Decisions-so-far / Fog.
- **Child ticket**: a sub-issue of the map, in the repo the question is about, labelled `wayfinder:<type>` (`research`/`prototype`/`grilling`/`task`).
- **Blocking**: native dependencies, across repos: `gh api --method POST repos/bp-bitplane/<repo>/issues/<child>/dependencies/blocked_by -F issue_id=<blocker-db-id>`. Unblocked when every blocker is closed.
- **Frontier**: `gh api repos/bp-bitplane/bitplane-protocol/issues/<map>/sub_issues`; keep open, unassigned children with `issue_dependencies_summary.blocked_by == 0`; first in map order wins.
- **Claim**: `gh issue edit <n> -R bp-bitplane/<repo> --add-assignee @me`, the session's first write.
- **Resolve**: comment the answer, close, then append a gist + link to the map's Decisions-so-far.
