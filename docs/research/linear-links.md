# Linear issue links: research notes

Researched 2026-10-13. Sources are Linear's help center, developer docs, the published GraphQL schema (`packages/sdk/src/schema.graphql` in `linear/linear`, last changed 2026-10-14T03:27Z), the association files Linear serves at `linear.app/.well-known/`, and this machine's registry. Anything not confirmed by a primary source is marked **unverified**.

## 1. Opening an issue in the desktop app

- Linear documents a `linear://` scheme: "using a `linear://` protocol followed by the rest of the URL will open that page in the desktop app instead of a browser. Example: `linear://linear.app/issue/ENG-123`" (FAQ). Source: https://linear.app/docs/get-the-app
- So the documented form is the https URL with `https://` replaced by `linear://`, for example `linear://linear.app/leftos/issue/RT-116`. Source: https://linear.app/docs/get-the-app
- A shorter form `linear://{workspace}/issue/{ID}` is reported working by users (no `linear.app` host). **Unverified** by Linear; prefer the documented form. Source: https://github.com/linear/linear/issues/44
- The desktop app is available for macOS (Intel and Apple Silicon) and Windows. Source: https://linear.app/docs/get-the-app
- Windows registration, observed on this machine (Linear desktop installed per-user): `HKCU\Software\Classes\linear` has `(Default) = URL:linear`, an empty `URL Protocol` value, and `shell\open\command = "C:\Users\lefto\AppData\Local\Programs\Linear\Linear.exe" "%1"`. No `HKLM\Software\Classes\linear` key exists, so the registration is per-user and shows through the merged `HKCR\linear` view. Source: local `reg query`, 2026-10-13.
- That key layout is the standard Windows URL-protocol registration (`URL Protocol` value plus `shell\open\command`). Source: https://learn.microsoft.com/en-us/previous-versions/windows/internet-explorer/ie-developer/platform-apis/aa767914(v=vs.85)
- Detection: a caller can treat the scheme as registered when `HKCR\linear` (or `HKCU\Software\Classes\linear`) has a `URL Protocol` value and a `shell\open\command` whose target exe exists; otherwise fall back to the https URL. The exe-exists check is a design suggestion, not a Linear statement.
- macOS: the app's bundle id is `com.linear` (used in Linear's own `defaults write ... com.linear` instructions). Source: https://linear.app/docs/get-the-app . A caller can ask Launch Services for the handler of `linear://` (`NSWorkspace.urlForApplication(toOpen:)`), as Raycast's script does before falling back to https. Source: https://github.com/raycast/script-commands/blob/master/commands/navigation/open-desktop-url-from-clipboard.swift
- Web-to-desktop redirect: a browser preference, "Open in desktop app" (Account > Preferences > Behavior, also described as Settings > General > Desktop application), makes linear.app links in the browser open in the desktop app if installed. Source: https://linear.app/docs/get-the-app ; changelog: https://linear.app/changelog/2021-11-18-front-integration-and-universal-links
- The web app detects the desktop app by probing localhost ports `44450`, `18450` and `33234`; browsers that block Local Network Access (or Brave Shields) break the redirect. Source: https://linear.app/docs/get-the-app
- Consequence for a desktop client: opening the https URL in the default browser only reaches the desktop app if the user enabled that preference and the browser allows the localhost probe; launching `linear://...` directly skips the browser.

## 2. Opening on phones

- iOS universal links: `https://linear.app/.well-known/apple-app-site-association` lists app `7VZ2S3V9RV.com.linear.ios` with path components including `*/issue/*`, `*/team/*`, `*/project/*`, `*/view/*`, `*/document/*`, `*/inbox/*` and others, and excludes URLs whose fragment is `no_universal_links`. So `https://linear.app/{workspace}/issue/{ID}/...` opens the iOS app when installed. Source: https://linear.app/.well-known/apple-app-site-association (fetched 2026-10-13)
- Android App Links: `https://linear.app/.well-known/assetlinks.json` grants `delegate_permission/common.handle_all_urls` to package `app.linear` (plus internal builds). Whether the app's manifest filters cover issue paths is **unverified**, but the association file is the verification half of App Links. Source: https://linear.app/.well-known/assetlinks.json ; mechanism: https://developer.android.com/training/app-links/verify-android-applinks
- Custom URL schemes registered by the mobile apps: **unverified** (no Linear documentation found). For a planned Android client, opening the https URL with an `ACTION_VIEW` intent is the documented-safe route.

## 3. Web URL form

- Issue URLs take the form `https://linear.app/{workspace urlKey}/issue/{IDENTIFIER}/{slug}`; the iOS association pattern `*/issue/*` matches the workspace segment before `issue`. Source: https://linear.app/.well-known/apple-app-site-association
- The `Issue.url` field ("Issue URL") returns the canonical URL, so a client can take it from the API rather than build it. Source: schema, `type Issue` field `url: String!`, https://raw.githubusercontent.com/linear/linear/master/packages/sdk/src/schema.graphql
- Whether the trailing slug is optional: **unverified** in docs; users report `linear://{team}/issue/{id}` without slug opening correctly, and Linear's own FAQ example omits both slug and workspace (`linear://linear.app/issue/ENG-123`). Source: https://linear.app/docs/get-the-app , https://github.com/linear/linear/issues/44
- Omitting the workspace key: only Linear's FAQ example shows it (`linear.app/issue/ENG-123`); behaviour for https URLs without it, and for users in several workspaces, is **unverified**.
- Workspace key from the API: `Organization.urlKey` ("The workspace's unique URL key, used in URLs to identify the workspace"), reachable as `{ organization { urlKey } }` or `{ viewer { organization { urlKey } } }`. Source: schema, `type Organization`.

## 4. Identifier format

- An identifier is team key + `-` + number: `Issue.number` is "The issue's unique number, scoped to the issue's team. Together with the team key, this forms the issue's human-readable identifier (e.g., ENG-123)". Source: schema, `type Issue`.
- `Team.key`: "The team's unique key, used as a prefix in issue identifiers (e.g., 'ENG' in 'ENG-123') and in URLs." Source: schema, `type Team`.
- `Issue.previousIdentifiers` lists old identifiers after a move between teams, so an old ID in a terminal may still resolve. Source: schema, `type Issue`.
- Allowed characters and length of a team key: **unverified**. No Linear doc or schema description states them; a third-party tool claims "2-5 uppercase letters, alphanumeric", which is not a Linear statement. Source: https://aident.ai/actions/linear_tools/linear_create_team . A detector should rather use the workspace's actual keys (below) than a guessed charset.
- Listing team keys: `{ teams(first: 250) { nodes { key name } } }`; `TeamFilter` also has a `key` comparator. Source: schema, `type Team`, `input TeamFilter`; query example at https://linear.app/developers/graphql

## 5. API for a hover card

- Endpoint `https://api.linear.app/graphql`; schema introspection is supported. Source: https://linear.app/developers/graphql
- `issue(id: String!)` accepts the shorthand identifier: the docs show `issue(id: "BLA-123")`, and state for `issueUpdate` that the id "can be either be the uuid ... or the shorthand id like `BLA-123`". Source: https://linear.app/developers/graphql
- Fields on `Issue` usable on a compact card (all present in the schema): `identifier`, `title`, `url`, `branchName`, `state { name type color }`, `priority`, `priorityLabel`, `assignee { name displayName avatarUrl }`, `project { name }`, `projectMilestone { name }`, `cycle { number name }`, and more listed in the next item. Source: schema, `type Issue`.
- More card fields: `labels { nodes { name color } }`, `estimate`, `dueDate`, `updatedAt`, `completedAt`, `releases(...)`, `attachments(...) { nodes { title url sourceType } }`, `parent { identifier title }`, `children(...)` (sub-issues; no dedicated count field found), `team { key }`. Source: schema, `type Issue`.
- Sub-field names inside related types other than `Issue` are **unverified** here; check them by introspection.
- Batching: GraphQL aliases allow several `issue(id:)` calls in one request (standard GraphQL; not specifically documented by Linear). Alternatively `issues(filter: { team: { key: { eq: "RT" } }, number: { in: [116, 117] } })` uses `IssueFilter.number` and `team` filters. Source: schema, `input IssueFilter`. Exact comparator support for `number: { in: [...] }` is **unverified**.
- `issueVcsBranchSearch(branchName:)` finds an issue from a git branch name. Source: schema, `type Query`.
- `searchIssues` is limited to 30 requests per minute. Source: schema, `type Query`.
- Rate limits (leaky bucket): API key 2,500 requests/hour per user, OAuth app 5,000/hour per user, unauthenticated 600/hour per IP. Complexity: API key 3,000,000 points/hour, OAuth app 2,000,000, unauthenticated 100,000; any single query over 10,000 points is rejected. Source: https://linear.app/developers/rate-limiting
- Each property is 0.1 point, each object 1, and connections multiply children by `first` (default 50), so card queries should pass small `first:` values. Headers: `X-RateLimit-Requests-*`, `X-Complexity`, `X-RateLimit-Complexity-*`. Source: https://linear.app/developers/rate-limiting
- Linear "especially discourage[s]" polling and points to webhooks. Source: https://linear.app/developers/rate-limiting
- Webhooks need a public HTTPS non-localhost URL and admin rights (or an OAuth app with `admin` scope), so they do not fit a purely local desktop client. Source: https://linear.app/developers/webhooks
- The schema has a `Subscription` type (`issueUpdated`, `issueCreated`, ...), but Linear's developer docs do not document a public subscription transport; treat GraphQL subscriptions as **unverified / not supported for third parties**. Source: schema, `type Subscription`.
- Practical consequence: fetch on hover, cache by identifier with `updatedAt`, and refresh lazily.

## 6. Auth

- Personal API keys: header `Authorization: <API_KEY>` (no `Bearer`). OAuth access tokens: `Authorization: Bearer <ACCESS_TOKEN>`. Source: https://linear.app/developers/graphql
- Keys are created at Settings > Account > Security & Access; each key can have full access or be restricted to Read, Write, Admin, Create issues, Create comments, and can be limited to specific teams. Admins can stop Members creating keys. Source: https://linear.app/docs/api-and-webhooks , https://linear.app/docs/security-and-access
- A read-only card needs only the Read permission on a key, or OAuth scope `read` (default, always present). Source: https://linear.app/docs/api-and-webhooks , https://linear.app/developers/oauth-2-0-authentication
- Per-workspace: keys are listed and revoked per workspace (Settings > Administration > API) and can be limited to that workspace's teams, which implies a key belongs to one workspace; an explicit statement is **unverified**. Request quotas are per user and shared across that user's keys. Source: https://linear.app/docs/api-and-webhooks , https://linear.app/developers/rate-limiting
- OAuth supports PKCE (`code_challenge`, `code_challenge_method=S256`), and redirect URIs such as `http://localhost:3000/oauth/callback` appear in Linear's examples; all OAuth apps use refresh tokens since 2026-04-01. Whether a PKCE token exchange can omit `client_secret` is **unverified** (the page lists different parameters for PKCE, truncated in the fetch). Source: https://linear.app/developers/oauth-2-0-authentication
- For a single-owner local tool, a personal API key with Read permission is the simplest option; OAuth matters once others install the app.
