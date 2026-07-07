import React from 'react'
import { renderToString } from 'react-dom/server';

interface RecursiveDivProps {
  deep: number;
  count: number;
}

const RecursiveDiv: React.FC<RecursiveDivProps> = ({ deep, count }) => {
  if (deep === 0) {
    return null;
  }
  const children = Array.from({ length: count }, (_, index) => (
    <RecursiveDiv key={index} deep={deep - 1} count={count} />
  ));
  const id = [deep, count].join('-')
  return <div
    id={id}
    key={id}
  >{children}</div>;
};

const st = +Date.now()
const s = renderToString(
  <RecursiveDiv deep={4} count={8} />
)
const ed = +Date.now()
console.log("ssr string: ", s)
console.log("ssr time: ", ed - st)
