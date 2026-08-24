// SPDX-License-Identifier: BSD-3-Clause

interface ViewProperties {
  label: string;
}

const properties: ViewProperties = { label: "typed" };
export default <output>{properties.label}</output>;
