import { hooks } from "./hooks";
import { openHardwareEditor } from "./hardware-editor";

/// Back-compat entry point used by the VMs tab button. It no longer switches
/// the app to another daemon when none is open: the form takes the connection
/// it is given and the view stays where it was.
export function openCreateVm() {
  hooks.dcCreate((conn: string) => void openHardwareEditor({ conn }));
}
