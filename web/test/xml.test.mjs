// Network XML builder/parser: modes emit the right elements and the edit
// form round-trips the live definition.
import assert from "node:assert/strict";
import test, { before } from "node:test";

let xml;

before(async () => {
  xml = await import("./_build/xml.js");
});

const form = (over = {}) => ({
  name: "net0",
  forward: "nat",
  bridge: "",
  ip: "",
  dhcp_start: "",
  dhcp_end: "",
  ...over,
});

test("NAT networks emit a forward and their own subnet, no bridge", () => {
  const out = xml.networkXml(form({
    ip: "192.168.100.1/24",
    dhcp_start: "192.168.100.10",
    dhcp_end: "192.168.100.200",
  }));
  assert.match(out, /<forward mode="nat">/);
  assert.match(out, /<ip address="192\.168\.100\.1" netmask="255\.255\.255\.0">/);
  assert.match(out, /<range start="192\.168\.100\.10" end="192\.168\.100\.200"/);
  assert.doesNotMatch(out, /<bridge /);
});

test("bridge networks attach to a bridge and define no ip", () => {
  const out = xml.networkXml(form({ forward: "bridge", bridge: "br0", ip: "10.0.0.1/24" }));
  assert.match(out, /<forward mode="bridge"\/>/);
  assert.match(out, /<bridge name="br0"\/>/);
  assert.doesNotMatch(out, /<ip /, "bridge networks must not define their own subnet");
});

test("isolated networks have no forward and may carry an ip", () => {
  const out = xml.networkXml(form({ forward: "none", ip: "10.9.0.1/24" }));
  assert.doesNotMatch(out, /<forward /);
  assert.doesNotMatch(out, /<bridge /);
  assert.match(out, /<ip address="10\.9\.0\.1"/);
});

test("the edit form round-trips a definition, preserving uuid", () => {
  const original = xml.networkXml(form({
    name: "existing",
    forward: "nat",
    ip: "192.168.77.1/24",
    dhcp_start: "192.168.77.50",
    dhcp_end: "192.168.77.99",
  }), "abc-123");
  const parsed = xml.networkFormFromXml(original);
  assert.equal(parsed.name, "existing");
  assert.equal(parsed.forward, "nat");
  assert.equal(parsed.ip, "192.168.77.1/24", "CIDR rebuilt from the netmask");
  assert.equal(parsed.dhcp_start, "192.168.77.50");
  assert.equal(parsed.dhcp_end, "192.168.77.99");
  assert.equal(xml.networkUuid(original), "abc-123");
});

test("a missing netmask still yields the bare address", () => {
  const out = xml.networkXml(form({ forward: "nat", ip: "10.1.2.3/24" }));
  const parsed = xml.networkFormFromXml(out);
  assert.equal(parsed.ip, "10.1.2.3/24");
});
