// process for the converter in the browser: no console to ask, no environment.
export const process = { env: {}, argv: [], platform: 'win32', stdin: { isTTY: false }, stdout: { write() {} }, cwd: () => '/' };
export default process;
