import {
  IAggregationIsm,
  IInterchainSecurityModule,
  IMultisigIsm,
  IRoutingIsm,
  OPStackIsm,
  StaticMerkleRootMultisigIsm,
  StaticMessageIdMultisigIsm,
  TestIsm,
} from '@hyperlane-xyz/core';
import type { Address } from '@hyperlane-xyz/utils';

import { ChainMap } from '../types';

export type DeployedIsm =
  | IInterchainSecurityModule
  | IMultisigIsm
  | IAggregationIsm
  | IRoutingIsm
  | StaticMessageIdMultisigIsm
  | StaticMerkleRootMultisigIsm
  | OPStackIsm
  | TestIsm

export enum ModuleType {
  UNUSED,
  ROUTING,
  AGGREGATION,
  LEGACY_MULTISIG, // DEPRECATED
  MERKLE_ROOT_MULTISIG,
  MESSAGE_ID_MULTISIG,
  OP_STACK,
  NULL,
}

export type MultisigConfig = {
  validators: Array<Address>;
  threshold: number;
};

export type MultisigIsmConfig = MultisigConfig & {
  type: ModuleType.MERKLE_ROOT_MULTISIG | ModuleType.MESSAGE_ID_MULTISIG;
};

export type TestIsmConfig = {
  type: ModuleType.NULL;
};

export type RoutingIsmConfig = {
  type: ModuleType.ROUTING;
  owner: Address;
  domains: ChainMap<IsmConfig>;
};

export type AggregationIsmConfig = {
  type: ModuleType.AGGREGATION;
  modules: Array<IsmConfig>;
  threshold: number;
};

export type OpStackIsmConfig = {
  type: ModuleType.OP_STACK;
  nativeBridge: Address;
};

export type IsmConfig =
  | Address
  | RoutingIsmConfig
  | MultisigIsmConfig
  | AggregationIsmConfig
  | OpStackIsmConfig
  | TestIsmConfig
