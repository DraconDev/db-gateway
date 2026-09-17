import type { SqlValue, Statement } from '../src/index.js';

const values: SqlValue[] = [null, true, 42, 1.5, 'text', [0, 128, 255]];
const statement: Statement = { sql: 'SELECT ?', args: values };
void statement;
// @ts-expect-error Integer wrapper objects are not supported by the server.
const integerObject: SqlValue = { integer: '42' };
// @ts-expect-error Arbitrary object arguments are not SQL values.
const objectStatement: Statement = { sql: 'SELECT ?', args: [{}] };
void integerObject;
void objectStatement;
