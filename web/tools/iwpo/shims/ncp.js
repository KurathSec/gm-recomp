// ncp (Utils.copyDir) over the in-memory file system.
import { copyDir } from './fs.js';

export function ncp(src, dst, cb) {
  try {
    copyDir(src, dst);
    cb(null);
  } catch (e) {
    cb(e);
  }
}
export default ncp;
