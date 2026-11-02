// @sectr/copilotkit — run CopilotKit on Sectr.
//
// `SectrAgent` is the AG-UI agent for a Sectr route: it runs turns and
// reconnects a reloaded page. Hand `SectrRunner` to your Copilot Runtime and
// every run and reconnect streams from Sectr with the user's token; or use a
// `SectrAgent` straight from the browser (direct mode).
// Guide: https://docs.sectr.dev/getting-started/connecting-frontend/
export { SectrAgent } from "./agent.js";
export { SectrRunner } from "./runner.js";
