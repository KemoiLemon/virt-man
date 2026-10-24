// Stand-in for @novnc/novnc in the VM_IIFE smoke build (see vite.config.ts).
export default class RFB extends EventTarget {
  constructor(target, url) {
    super();
    this.target = target;
    this.url = url;
    this.scaleViewport = false;
    setTimeout(() => this.dispatchEvent(new Event("connect")), 0);
  }
  sendKey() {}
  sendCtrlAltDel() {}
  focus() {}
  disconnect() {}
}
