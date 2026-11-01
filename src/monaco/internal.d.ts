// Types for the few Monaco internals RustCode uses (Monaco ships no
// declarations for them). Keep in sync when upgrading Monaco.

declare module 'monaco-esm/editor/browser/services/bulkEditService.js' {
  /** The service identifier of VS Code's `IBulkEditService` ('IWorkspaceEditService'). */
  export const IBulkEditService: { toString(): string };
}

declare module 'monaco-esm/editor/standalone/browser/standaloneServices.js' {
  export interface INotification {
    severity: number;
    message: string | Error;
  }
  export class StandaloneNotificationService {
    notify(notification: INotification): unknown;
  }
  export const StandaloneServices: {
    initialize(overrides: Record<string, unknown>): unknown;
    get(id: { toString(): string }): unknown;
  };
}

declare module 'monaco-esm/editor/common/services/editorWorker.js' {
  /** The service identifier of `IEditorWorkerService`. */
  export const IEditorWorkerService: { toString(): string };
}
