// A stand-in for `document.modelContext`.
//
// Real WebMCP needs Chrome 149 with the origin trial, which a CI runner does
// not have. These fixtures exist to test *this crate's* acquisition and settle
// logic — that it waits long enough, tells "no API" apart from "no tools", and
// keys the contract correctly — not Chrome's implementation of the proposal.
// The shape below is the part of the API a scan touches, and nothing more.
(() => {
  const tools = [];
  Object.defineProperty(document, 'modelContext', {
    configurable: true,
    value: {
      registerTool(tool) {
        tools.push(tool);
      },
      unregisterAll() {
        tools.length = 0;
      },
      async getTools() {
        // `execute` is a function, so it would not survive serialisation to
        // the scanner anyway. Dropped here so the fixture matches what a real
        // page's descriptors look like on the wire.
        return tools.map(({ execute, ...rest }) => rest);
      },
    },
  });
})();
