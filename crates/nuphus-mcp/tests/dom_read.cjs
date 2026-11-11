'use strict';
const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const path = require('node:path');
const source = fs.readFileSync(path.resolve(__dirname, '../src/tools/dom_read.js'), 'utf8');
const read = vm.runInNewContext('(' + source + ')');
const cases = [];
function check(name, fn) { fn(); cases.push({name, passed: true}); }
function sample(value, overrides={}) {
  const element = {innerText:value, textContent:value};
  const root = {querySelector:()=>element};
  return read.call({closest:()=>null,ownerDocument:root}, {selector:'#output', max_chars:100, until_text:'', completion_prefix:'TASK_test_RC=',...overrides});
}
check('numeric zero receipt', ()=>assert.equal(sample('TASK_test_RC=0\n').exit_code,0));
check('nonzero receipt preserved', ()=>assert.equal(sample('TASK_test_RC=13\n').exit_code,13));
check('maximum shell status', ()=>assert.equal(sample('TASK_test_RC=255\n').exit_code,255));
check('out-of-range status rejected', ()=>assert.equal(sample('TASK_test_RC=256\n').matched,false));
check('partial digit is not a finished line', ()=>assert.equal(sample('TASK_test_RC=1').matched,false));
check('CRLF receipt accepted', ()=>assert.equal(sample('TASK_test_RC=13\r\n').exit_code,13));
check('leading zero is not generated shell status', ()=>assert.equal(sample('TASK_test_RC=013\n').matched,false));
check('echoed command does not satisfy exact receipt', ()=>assert.equal(sample('printf "TASK_test_RC=0"\n').matched,false));
check('receipt must begin independent line', ()=>assert.equal(sample('log TASK_test_RC=0\n').matched,false));
check('receipt must end independent line', ()=>assert.equal(sample('TASK_test_RC=0 extra\n').matched,false));
check('older job cannot satisfy current marker', ()=>assert.equal(sample('TASK_old_RC=0\n').matched,false));
check('duplicate receipts are ambiguous', ()=>assert.equal(sample('TASK_test_RC=0\nTASK_test_RC=0\n').ambiguous,true));
check('conflicting receipts are ambiguous', ()=>assert.equal(sample('TASK_test_RC=0\nTASK_test_RC=13\n').matched,false));
check('marker matched before output truncation', ()=>{
  const result=sample('TASK_test_RC=0\n'+'z'.repeat(500), {max_chars:3});
  assert.equal(result.exit_code,0); assert.equal(result.result,'zzz'); assert.equal(result.truncated,true);
});
check('legacy substring matched before truncation', ()=>{
  const result=sample('done\n'+'z'.repeat(500), {max_chars:3,completion_prefix:'',until_text:'done'});
  assert.equal(result.matched,true); assert.equal(result.result,'zzz');
});
check('Unicode tail retains complete code points', ()=>{
  const result=sample('A中文😀',{max_chars:1,completion_prefix:''});
  assert.equal(result.result,'😀'); assert.equal(result.total_chars,4);
});
check('missing DOM target is explicit error', ()=>assert.throws(()=>read.call({closest:()=>null,ownerDocument:{querySelector:()=>null}}, {selector:'#missing'}),/target not found/));
check('terminal scope wins over document', ()=>{
  const terminal={querySelector:()=>({innerText:'TASK_test_RC=0\n'})};
  const result=read.call({closest:()=>terminal,ownerDocument:{querySelector:()=>{throw Error('wrong scope');}}},{selector:'.xterm-accessibility-tree',max_chars:100,completion_prefix:'TASK_test_RC='});
  assert.equal(result.exit_code,0);
});
console.log(JSON.stringify({passed:cases.length,failed:0,cases},null,2));
