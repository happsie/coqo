import React from "react";

interface Props {
  title: string;
}

export function Card({ title }: Props) {
  return (
    <div className="card">
      <h1>{title}</h1>
    </div>
  );
}

export const List = ({ items }: { items: string[] }) => (
  <ul>
    {items.map((item) => (
      <li key={item}>{item}</li>
    ))}
  </ul>
);
