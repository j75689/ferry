# Releasing

CI bundles the desktop app when it runs from a `v*` tag or by hand
(*Actions → CI → Run workflow*). A tag also publishes the bundles as a
GitHub pre-release.

## macOS signing and notarization

Without Apple credentials the macOS bundles are ad-hoc signed, and people
have to right-click → Open the app the first time. To ship a build that
opens normally, add these repository secrets (*Settings → Secrets and
variables → Actions*). CI then signs with your Developer ID, notarizes, and
staples the ticket, and the *Verify the macOS signature* step fails the
build if any of that did not happen.

**Signing** needs a *Developer ID Application* certificate from an Apple
Developer Program membership.

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | The certificate and its private key exported from Keychain Access as `.p12`, base64-encoded: `base64 -i ferry.p12 \| pbcopy` |
| `APPLE_CERTIFICATE_PASSWORD` | The password chosen when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | The certificate's name, for example `Developer ID Application: Jane Doe (ABCDE12345)` (`security find-identity -v -p codesigning` lists it) |

**Notarization** takes either an App Store Connect API key (preferred):

| Secret | Value |
|---|---|
| `APPLE_API_KEY` | The key ID |
| `APPLE_API_ISSUER` | The issuer ID shown above the key list |
| `APPLE_API_KEY_P8` | The contents of the downloaded `AuthKey_<id>.p8` file |

or an Apple ID with an app-specific password:

| Secret | Value |
|---|---|
| `APPLE_ID` | The Apple ID email |
| `APPLE_PASSWORD` | An app-specific password from appleid.apple.com |
| `APPLE_TEAM_ID` | The team ID, the code in parentheses in the signing identity |

Signing without notarization works too, but Gatekeeper still warns on first
launch, so set both. The `ferry` CLI inside the app bundle is signed with the
app.
