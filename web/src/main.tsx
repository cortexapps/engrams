import { captureMcpCallback } from "./mcp-callback-bootstrap";

const callback = captureMcpCallback(new URL(window.location.href), window.history);
if (callback === undefined) {
  void import("./app-entry");
} else {
  void import("./mcp-callback").then(({ completeCallback }) => completeCallback(callback));
}
