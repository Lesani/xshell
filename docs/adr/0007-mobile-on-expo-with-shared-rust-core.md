# The Mobile is an Expo app over the shared Rust core, not Tauri mobile

The Desktop is Tauri, so Tauri 2 mobile looks like the obvious choice for the Mobile. We chose React Native with Expo because the Mobile's paid features (remote push through APNs/FCM, StoreKit 2 and Play Billing subscriptions) and its phone ergonomics are exactly where Tauri's mobile plugin ecosystem is thinnest and Expo's is mature. The protocol codec, Pairing, Roster verification and end-to-end crypto stay in Rust and are shared with the Desktop and Daemon (via uniffi-bindgen-react-native), so the security layer has one implementation; the Terminal View is xterm.js in a WebView.

## Considered Options

- **Tauri 2 mobile.** Rejected for now: reuses the React components, but push, in-app purchases and mobile polish would all be hand-written Swift/Kotlin plugins.
- **Native Swift and Kotlin over the Rust core.** Rejected: two UIs for one maintainer.
