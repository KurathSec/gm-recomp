// Modules the GM8 converter imports but never uses in the browser (building its DLL,
// asking on the console, the update check).
const unavailable = () => {
  throw new Error('not available in the browser');
};
export const exec = unavailable;
export const spawn = unavailable;
export const createInterface = unavailable;
export const get = unavailable;
export const request = unavailable;
export default { exec, spawn, createInterface, get, request };
