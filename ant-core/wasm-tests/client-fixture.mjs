// Test callers persist checkpoints just as the SDK does; production bindings stay unwrapped.
import { BrowserNetworkClient as RawBrowserNetworkClient } from "./pkg/ant_core.js";
export class BrowserNetworkClient extends RawBrowserNetworkClient {
  uploadPublicFile(...args) {
    args[7] ??= value => { this.lastCheckpoint = value; };
    return super.uploadPublicFile(...args);
  }
  uploadStagedPublicFile(...args) {
    args[6] ??= value => { this.lastCheckpoint = value; };
    return super.uploadStagedPublicFile(...args);
  }
  uploadRecords(...args) {
    args[6] ??= value => { this.lastCheckpoint = value; };
    return super.uploadRecords(...args);
  }
}
